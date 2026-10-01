import { create } from "@bufbuild/protobuf";
import { Code, ConnectError, type ServiceImpl } from "@connectrpc/connect";

import { newId } from "../crypto.ts";
import { createDeployment } from "../deployments/store.ts";
import type { Deps } from "../deps.ts";
import { SleepingPingMode } from "../gen/chunk/management/v1/common_pb.ts";
import { DeploymentTrigger } from "../gen/chunk/management/v1/deployments_pb.ts";
import {
  EnvironmentState,
  ForkEnvironmentResponseSchema,
  ProjectService,
  SnapshotSchema,
} from "../gen/chunk/management/v1/projects_pb.ts";
import type { LogStoreIssuer } from "../logstore/issuer.ts";
import { listSnapshots, olderThan, parseSnapshotId, snapshotId, type StoredSnapshot } from "../logstore/s3.ts";
import { callerOf } from "../rpc/caller.ts";
import { idempotent } from "../rpc/idempotency.ts";
import { failedPrecondition, invalid, notFound, page, pageOf, slug, timestamp, unique } from "../rpc/validate.ts";
import { copySecrets } from "../secrets/service.ts";
import { type EnvironmentRow, hostnameOf, loadEnvironment, toEnvironment } from "./store.ts";

/** How long listing an environment's snapshots may take. */
const listTimeoutMs = 30_000;

export function forkHandlers({
  sql,
  keys,
  edge,
  logStore,
  jvmImage,
}: Deps): Pick<ServiceImpl<typeof ProjectService>, "forkEnvironment" | "listSnapshots"> {
  return {
    /**
     * The fork restores from the source's log until its core first attaches, so a deleted source's log is kept until
     * then (see the reconciler). A snapshot chosen here may still be pruned before the fork first starts, which then
     * fails to start.
     */
    async forkEnvironment(request, context) {
      const caller = callerOf(context);
      const name = slug(request.name, "name");
      const method = ProjectService.method.forkEnvironment;
      return idempotent({ sql, keys, caller, method, request }, async (tx) => {
        if (!logStore) throw failedPrecondition("forks need log replication, which this install has turned off");
        const source = await loadEnvironment(tx, caller, request.sourceEnvironmentId, {
          field: "source_environment_id",
        });
        const snapshots = await storedSnapshots(logStore, source.id);
        if (request.snapshotId && !snapshots.some((snapshot) => snapshotId(snapshot) === request.snapshotId)) {
          throw notFound("snapshot");
        }
        if (snapshots.length === 0) throw failedPrecondition("the source environment has no snapshot yet");
        // Held until the fork exists, so the source can't start deleting its log before then.
        const [locked] = await tx<EnvironmentRow[]>`select * from environments where id = ${source.id} for share`;
        if (!locked || locked.state === EnvironmentState.DELETING) {
          throw failedPrecondition("the source environment is being deleted");
        }
        const [active] = await tx<{ release_id: string }[]>`
          select release_id from deployments where id = ${locked.active_deployment_id}`;
        if (!active) throw failedPrecondition("the source environment has no active deployment");
        const id = newId("env");
        const [row] = await unique(
          "an environment with this name already exists in the project",
          () =>
            tx<EnvironmentRow[]>`
            insert into environments (id, project_id, name, state, sleeping_ping, hostname, forked_from_environment_id,
              forked_from_snapshot_id)
            values (${id}, ${locked.project_id}, ${name}, ${EnvironmentState.PENDING}, ${SleepingPingMode.CACHE},
              ${hostnameOf(id, edge)}, ${locked.id}, ${request.snapshotId})
            returning *`,
        );
        if (!row) throw new Error("environment insert returned no row");
        if (request.copySecrets) await copySecrets(tx, keys, locked.id, id);
        await createDeployment(tx, row, active.release_id, DeploymentTrigger.FORK, jvmImage);
        return create(ForkEnvironmentResponseSchema, { environment: toEnvironment(row, edge) });
      });
    },

    async listSnapshots(request, context) {
      const environment = await loadEnvironment(sql, callerOf(context), request.environmentId);
      const p = page(request);
      const after = p.after === undefined ? undefined : parseSnapshotId(p.after);
      if (p.after !== undefined && !after) throw invalid("page_token is not valid");
      if (!logStore) return { snapshots: [], nextPageToken: "" };
      const snapshots = await storedSnapshots(logStore, environment.id);
      const rows = after ? snapshots.filter((snapshot) => olderThan(snapshot, after)) : snapshots;
      const { items, nextPageToken } = pageOf(rows, p, snapshotId);
      return {
        snapshots: items.map((snapshot) =>
          create(SnapshotSchema, {
            id: snapshotId(snapshot),
            environmentId: environment.id,
            epoch: snapshot.epoch,
            logSequence: snapshot.sequence,
            createTime: timestamp(snapshot.createTime),
          }),
        ),
        nextPageToken,
      };
    },
  };
}

/** The snapshots stored in the environment's log, newest first. */
async function storedSnapshots(logStore: LogStoreIssuer, environmentId: string): Promise<StoredSnapshot[]> {
  try {
    return await listSnapshots(await logStore.readGrant(environmentId), AbortSignal.timeout(listTimeoutMs));
  } catch (error) {
    console.error("listing snapshots failed:", error);
    throw new ConnectError("the log store could not be listed; try again", Code.Unavailable);
  }
}
