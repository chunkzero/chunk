import { create, equals } from "@bufbuild/protobuf";
import type { ServiceImpl } from "@connectrpc/connect";

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
import { routeTable } from "./routes.ts";

const keepaliveMs = 30_000;
/** Accepted wakes that advance an environment's revision, per minute. */
export const wakesPerMinute = 30;

export function edgeService({ sql, changes, shutdown }: Deps): Partial<ServiceImpl<typeof EdgeService>> {
  return {
    async *watchRoutes(_request, context) {
      const subscription = changes.subscribe((change) => change.kind === "environment");
      try {
        let revision = 1n;
        let routes = await routeTable(sql);
        yield create(WatchRoutesResponseSchema, { revision, reset: true, routes: [...routes.values()] });
        let sentAt = Date.now();
        const signal = streamSignal(context.signal, shutdown);
        while (!signal.aborted) {
          await subscription.next(keepaliveMs - (Date.now() - sentAt), signal);
          if (signal.aborted) break;
          const next = await routeTable(sql);
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

      const { outcome } = await sql.begin(async (tx) => {
        const [environment] = await tx<WakeRow[]>`
          select state, sleeping_ping, revision, report_desired_revision,
            wake_window_start > now() - interval '1 minute' as in_window, wake_count
          from environments where id = ${environmentId} for update`;
        if (!environment || environment.state === EnvironmentState.DELETING) throw notFound("environment");
        if (!(await desiredDeployment(tx, environmentId))) {
          throw failedPrecondition("nothing is deployed to the environment");
        }
        const [blocked] = await tx`
          select 1 from blocked_addresses
          where environment_id = ${environmentId} and address = ${address} and expire_time > now()`;
        if (blocked) return { outcome: WakeOutcome.BLOCKED };
        // Edges answer these pings from the cached status, so there is nothing to wake.
        if (request.reason === WakeReason.PING && environment.sleeping_ping === SleepingPingMode.CACHE) {
          return { outcome: WakeOutcome.AWAKE };
        }
        const accepted = environment.state === EnvironmentState.RUNNING ? WakeOutcome.AWAKE : WakeOutcome.WAKING;
        // No report reflects the current revision yet, so an earlier wake already invalidated the idle report.
        if (environment.report_desired_revision < environment.revision) return { outcome: accepted };
        const count = environment.in_window ? environment.wake_count : 0;
        if (count >= wakesPerMinute) return { outcome: WakeOutcome.THROTTLED };
        await tx`
          update environments set
            wake_count = ${count + 1},
            wake_window_start = case when ${environment.in_window} then wake_window_start else now() end
          where id = ${environmentId}`;
        await advanceRevision(tx, environmentId);
        return { outcome: accepted };
      });
      return { outcome };
    },
  };
}

interface WakeRow {
  state: EnvironmentState;
  sleeping_ping: SleepingPingMode;
  revision: bigint;
  report_desired_revision: bigint;
  in_window: boolean;
  wake_count: number;
}
