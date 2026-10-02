import { create } from "@bufbuild/protobuf";
import type { Timestamp } from "@bufbuild/protobuf/wkt";
import type { ServiceImpl } from "@connectrpc/connect";
import { and, eq, sql } from "drizzle-orm";

import { notify } from "../changes.ts";
import type { Db } from "../db.ts";
import { activateDeployment, deployRestored, failDeployment, progressDeployment } from "../deployments/store.ts";
import type { Deps } from "../deps.ts";
import { DeploymentState } from "../gen/chunk/management/v1/common_pb.ts";
import {
  EnvironmentService,
  type ReportStatusRequest,
  SetWakeAlarmResponseSchema,
} from "../gen/chunk/management/v1/environment_pb.ts";
import { EnvironmentState } from "../gen/chunk/management/v1/projects_pb.ts";
import { environmentOf } from "../rpc/caller.ts";
import { endIfShuttingDown, failedPrecondition, invalid, notFound, streamSignal } from "../rpc/validate.ts";
import { deployments, environments } from "../schema.ts";
import { capacityServices } from "./capacity.ts";
import { desiredState } from "./desired.ts";
import { reportServices } from "./reports.ts";
import { claimLease, fenceLease } from "./store.ts";

const keepaliveMs = 30_000;
const maxGateways = 64;
const maxPings = 64;
const maxStatusJsonBytes = 64 * 1024;
/**
 * What the pings may total. JSON can escape each byte as six (`\u0000`), so even then they take at most 3 MiB of the
 * 4 MiB RPC body cap, leaving room for the rest of the report.
 */
const maxPingBytes = 512 * 1024;
const gatewayPattern = /^(?:\[[0-9A-Fa-f:.]+\]|[A-Za-z0-9.-]+):(\d{1,5})$/;

export function environmentService(deps: Deps): Partial<ServiceImpl<typeof EnvironmentService>> {
  const { db, changes, shutdown } = deps;
  return {
    async *attach(request, context) {
      const environmentId = environmentOf(context);
      if (!request.instanceId || request.instanceId.length > 128) {
        throw invalid("instance_id must be set, and at most 128 characters");
      }
      // Subscribe before the first read, so a change between the two still wakes the stream.
      const subscription = changes.subscribe(
        (change) => change.kind === "environment" && change.environmentId === environmentId,
      );
      try {
        const restored = request.restoredDeploymentId;
        const firstClaim = restored
          ? (tx: Db) => deployRestored(tx, environmentId, restored, deps.jvmImage)
          : undefined;
        const lease = request.core
          ? await claimLease(db, environmentId, request.instanceId, request.epoch, firstClaim)
          : 0n;
        let sentRevision: bigint | undefined;
        let sentAt = 0;
        const signal = streamSignal(context.signal, shutdown);
        while (!signal.aborted) {
          const { message, lease: current } = await desiredState(deps, environmentId);
          if (request.core) fenceLease(current, lease);
          if (message.revision !== sentRevision || Date.now() - sentAt >= keepaliveMs) {
            message.lease = lease;
            yield message;
            sentRevision = message.revision;
            sentAt = Date.now();
          }
          await subscription.next(keepaliveMs - (Date.now() - sentAt), signal);
        }
        endIfShuttingDown(shutdown);
      } finally {
        subscription.close();
      }
    },

    async reportStatus(request, context) {
      const environmentId = environmentOf(context);
      const gateways = request.gatewayAddresses.map(gatewayAddress);
      if (gateways.length > maxGateways) throw invalid(`at most ${maxGateways} gateway_addresses are allowed`);
      const pings = pingsOf(request);
      await db.transaction(async (tx) => {
        const [environment] = await tx
          .select({ lease: environments.lease, report_sequence: environments.report_sequence })
          .from(environments)
          .where(eq(environments.id, environmentId))
          .for("update");
        if (!environment) throw notFound("environment");
        fenceLease(environment.lease, request.lease);
        if (request.sequence <= environment.report_sequence) return;
        if (request.deployment?.deploymentId) {
          const { deploymentId, state, message } = request.deployment;
          const [owned] = await tx
            .select({ one: sql`1` })
            .from(deployments)
            .where(and(eq(deployments.id, deploymentId), eq(deployments.environment_id, environmentId)));
          if (!owned) throw invalid("deployment.deployment_id is not a deployment of this environment");
          if (state === DeploymentState.IN_PROGRESS) await progressDeployment(tx, deploymentId);
          else if (state === DeploymentState.ACTIVE) await activateDeployment(tx, deploymentId);
          else if (state === DeploymentState.FAILED) await failDeployment(tx, deploymentId, message);
          else throw invalid("deployment.state must be IN_PROGRESS, ACTIVE or FAILED");
        }
        const logins = request.logins;
        // Gateways admit only players they authenticated, so each new login confirms one of the window's login wakes.
        await tx
          .update(environments)
          .set({
            gateway_addresses: gateways,
            pings,
            online_players: request.onlinePlayers,
            wake_count: sql`case
              when wake_window_start > now() - interval '1 minute'
                then greatest(wake_count - least(greatest(${logins} - report_logins, 0), wake_logins_pending), 0)
              else wake_count
            end`,
            wake_logins_pending: sql`case
              when wake_window_start > now() - interval '1 minute'
                then wake_logins_pending - least(greatest(${logins} - report_logins, 0), wake_logins_pending)
              else 0
            end`,
            report_logins: sql`greatest(report_logins, ${logins})`,
            report_sequence: request.sequence,
            report_desired_revision: request.desiredRevision,
            ready_to_suspend: request.readyToSuspend,
            state: sql`case
              when state in (${EnvironmentState.PENDING}, ${EnvironmentState.STARTING}) and ${gateways.length > 0}
                then ${EnvironmentState.RUNNING}
              else state
            end`,
          })
          .where(eq(environments.id, environmentId));
        await notify(tx, { kind: "environment", environmentId });
      });
      return {};
    },

    async setWakeAlarm(request, context) {
      const environmentId = environmentOf(context);
      const due = request.dueTime;
      if (due && (due.nanos < 0 || due.nanos > 999_999_999)) throw invalid("due_time.nanos is out of range");
      const alarm = await db.transaction(async (tx) => {
        const [stored] = await tx
          .select(alarmColumns)
          .from(environments)
          .where(eq(environments.id, environmentId))
          .for("update");
        if (!stored) throw notFound("environment");
        fenceLease(stored.lease, request.lease);
        const order =
          compare(request.epoch, stored.alarm_epoch) || compare(request.generation, stored.alarm_generation);
        if (order > 0) {
          const [updated] = await tx
            .update(environments)
            .set({
              alarm_epoch: request.epoch,
              alarm_generation: request.generation,
              alarm_due_seconds: due?.seconds ?? null,
              alarm_due_nanos: due?.nanos ?? null,
              alarm_fired: false,
            })
            .where(eq(environments.id, environmentId))
            .returning(alarmColumns);
          await notify(tx, { kind: "environment", environmentId });
          return updated ?? stored;
        }
        if (order === 0 && !sameDue(stored, due)) {
          throw failedPrecondition("this alarm (epoch, generation) is already stored with a different due_time");
        }
        return stored;
      });
      return create(SetWakeAlarmResponseSchema, {
        epoch: alarm.alarm_epoch,
        generation: alarm.alarm_generation,
        ...(alarm.alarm_due_seconds === null
          ? {}
          : { dueTime: { seconds: alarm.alarm_due_seconds, nanos: alarm.alarm_due_nanos ?? 0 } }),
      });
    },

    ...capacityServices(deps),
    ...reportServices(deps),
  };
}

const alarmColumns = {
  lease: environments.lease,
  alarm_epoch: environments.alarm_epoch,
  alarm_generation: environments.alarm_generation,
  alarm_due_seconds: environments.alarm_due_seconds,
  alarm_due_nanos: environments.alarm_due_nanos,
};

type AlarmRow = Pick<typeof environments.$inferSelect, keyof typeof alarmColumns>;

function compare(a: bigint, b: bigint): number {
  return a === b ? 0 : a > b ? 1 : -1;
}

function sameDue(stored: AlarmRow, due: Timestamp | undefined): boolean {
  if (!due) return stored.alarm_due_seconds === null;
  return stored.alarm_due_seconds === due.seconds && stored.alarm_due_nanos === due.nanos;
}

function gatewayAddress(address: string): string {
  const port = Number(gatewayPattern.exec(address)?.[1] ?? 0);
  if (port < 1 || port > 65_535) throw invalid(`gateway address ${JSON.stringify(address)} is not host:port`);
  return address;
}

/** The reported status JSON by normalized hostname. */
function pingsOf(request: ReportStatusRequest): Record<string, string> {
  if (request.pings.length > maxPings) throw invalid(`at most ${maxPings} pings are allowed`);
  const pings: Record<string, string> = {};
  let bytes = 0;
  for (const { hostname, statusJson } of request.pings) {
    if (!hostname) throw invalid("pings[].hostname is required");
    if (Buffer.byteLength(statusJson) > maxStatusJsonBytes) {
      throw invalid(`pings[].status_json must be at most ${maxStatusJsonBytes} bytes`);
    }
    bytes += Buffer.byteLength(hostname) + Buffer.byteLength(statusJson);
    if (bytes > maxPingBytes) throw invalid(`pings must total at most ${maxPingBytes} bytes`);
    pings[normalizeHostname(hostname)] = statusJson;
  }
  return pings;
}

/** Lowercase without a trailing dot, the form routes and edges match on. */
export function normalizeHostname(hostname: string): string {
  return hostname.toLowerCase().replace(/\.$/, "");
}
