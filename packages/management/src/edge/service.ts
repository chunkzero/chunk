import { create, equals } from "@bufbuild/protobuf";
import type { ServiceImpl } from "@connectrpc/connect";
import { and, eq, gt, sql } from "drizzle-orm";

import type { Deps } from "../deps.ts";
import { blockKey } from "../environments/blocklist.ts";
import { desiredDeployment } from "../environments/desired.ts";
import { advanceRevision } from "../environments/store.ts";
import { SleepingPingMode } from "../gen/chunk/management/v1/common_pb.ts";
import {
  type EdgeService,
  RouteSchema,
  WakeOutcome,
  WakeReason,
  WatchRoutesResponseSchema,
} from "../gen/chunk/management/v1/edge_pb.ts";
import { EnvironmentState } from "../gen/chunk/management/v1/projects_pb.ts";
import { endIfShuttingDown, failedPrecondition, invalid, notFound, required, streamSignal } from "../rpc/validate.ts";
import { blockedAddresses, environments } from "../schema.ts";
import { routeTable } from "./routes.ts";

const keepaliveMs = 30_000;
/** Accepted wakes that advance an environment's revision, per minute, less login wakes a report confirms. */
export const wakesPerMinute = 30;

export function edgeService({ db, changes, shutdown }: Deps): Partial<ServiceImpl<typeof EdgeService>> {
  return {
    async *watchRoutes(_request, context) {
      const subscription = changes.subscribe((change) => change.kind === "environment");
      try {
        let revision = 1n;
        let routes = await routeTable(db);
        yield create(WatchRoutesResponseSchema, { revision, reset: true, routes: [...routes.values()] });
        let sentAt = Date.now();
        const signal = streamSignal(context.signal, shutdown);
        while (!signal.aborted) {
          await subscription.next(keepaliveMs - (Date.now() - sentAt), signal);
          if (signal.aborted) break;
          const next = await routeTable(db);
          const changed = [...next.values()].filter((route) => {
            const previous = routes.get(route.hostname);
            return !previous || !equals(RouteSchema, previous, route);
          });
          const removedHostnames = [...routes.keys()].filter((hostname) => !next.has(hostname));
          routes = next;
          // An empty message is the keepalive.
          if (changed.length === 0 && removedHostnames.length === 0 && Date.now() - sentAt < keepaliveMs) continue;
          revision += 1n;
          yield create(WatchRoutesResponseSchema, { revision, routes: changed, removedHostnames });
          sentAt = Date.now();
        }
        endIfShuttingDown(shutdown);
      } finally {
        subscription.close();
      }
    },

    async wake(request) {
      const environmentId = required(request.environmentId, "environment_id");
      if (request.reason !== WakeReason.LOGIN && request.reason !== WakeReason.PING) {
        throw invalid("reason must be LOGIN or PING");
      }
      const address = blockKey(required(request.clientAddress, "client_address"));
      if (address === undefined) throw invalid("client_address is not an IP address");

      const outcome = await db.transaction(async (tx) => {
        const [environment] = await tx
          .select({
            state: environments.state,
            sleeping_ping: environments.sleeping_ping,
            revision: environments.revision,
            report_desired_revision: environments.report_desired_revision,
            in_window: sql<boolean>`${environments.wake_window_start} > now() - interval '1 minute'`,
            wake_count: environments.wake_count,
          })
          .from(environments)
          .where(eq(environments.id, environmentId))
          .for("update");
        if (!environment || environment.state === EnvironmentState.DELETING) throw notFound("environment");
        if (!(await desiredDeployment(tx, environmentId))) {
          throw failedPrecondition("nothing is deployed to the environment");
        }
        const accepted = environment.state === EnvironmentState.RUNNING ? WakeOutcome.AWAKE : WakeOutcome.WAKING;
        const [blocked] = await tx
          .select({ one: sql`1` })
          .from(blockedAddresses)
          .where(
            and(
              eq(blockedAddresses.environment_id, environmentId),
              eq(blockedAddresses.address, address),
              gt(blockedAddresses.expire_time, sql`now()`),
            ),
          );
        // Blocked clients may reach a server that is already up or starting, but never wake a sleeping one.
        if (blocked) {
          return environment.state === EnvironmentState.SUSPENDED ? WakeOutcome.BLOCKED : accepted;
        }
        // Edges answer these pings from the cached status, so there is nothing to wake.
        if (request.reason === WakeReason.PING && environment.sleeping_ping === SleepingPingMode.CACHE) {
          return WakeOutcome.AWAKE;
        }
        // No report reflects the current revision yet, so an earlier wake already invalidated the idle report.
        if (environment.report_desired_revision < environment.revision) return accepted;
        const count = environment.in_window ? environment.wake_count : 0;
        if (count >= wakesPerMinute) return WakeOutcome.THROTTLED;
        // Only login wakes can be refunded, and only within the window they were counted in.
        const pending = request.reason === WakeReason.LOGIN ? 1 : 0;
        await tx
          .update(environments)
          .set({
            wake_count: count + 1,
            wake_window_start: sql`case when ${environment.in_window} then wake_window_start else now() end`,
            wake_logins_pending: sql`case when ${environment.in_window} then wake_logins_pending else 0 end + ${pending}`,
          })
          .where(eq(environments.id, environmentId));
        await advanceRevision(tx, environmentId);
        return accepted;
      });
      return { outcome };
    },
  };
}
