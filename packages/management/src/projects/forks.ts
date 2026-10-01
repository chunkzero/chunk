import { create } from "@bufbuild/protobuf";
import { Code, ConnectError, type ServiceImpl } from "@connectrpc/connect";

import { notify } from "../changes.ts";
import { newId } from "../crypto.ts";
import type { Deps } from "../deps.ts";
import { SleepingPingMode } from "../gen/chunk/management/v1/common_pb.ts";
import {
  EnvironmentState,
  type ForkEnvironmentRequest,
  ForkEnvironmentResponseSchema,
  ProjectService,
  SnapshotSchema,
} from "../gen/chunk/management/v1/projects_pb.ts";
import type { LogStoreIssuer } from "../logstore/issuer.ts";
import { listSnapshots, olderThan, parseSnapshotId, snapshotId, type StoredSnapshot } from "../logstore/s3.ts";
import { type Caller, callerOf } from "../rpc/caller.ts";
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
}: Deps): Pick<ServiceImpl<typeof ProjectService>, "forkEnvironment" | "listSnapshots"> {
  /** The source environment, once its log is known to hold the snapshot the request names, or any for its latest. */
  const sourceOf = async (request: ForkEnvironmentRequest, caller: Caller, signal: AbortSignal) => {
    if (!logStore) throw failedPrecondition("forks need log replication, which this install has turned off");
    const field = "source_environment_id";
    const source = await loadEnvironment(sql, caller, request.sourceEnvironmentId, { field });
    const snapshots = await storedSnapshots(logStore, source.id, signal);
    if (request.snapshotId && !snapshots.some((snapshot) => snapshotId(snapshot) === request.snapshotId)) {
      throw notFound("snapshot");
    }
    if (snapshots.length === 0) throw failedPrecondition("the source environment has no snapshot yet");
    return source;
  };

  return {
    /**
     * The fork starts without a deployment and restores from the source's log until its core first attaches, so a
     * deleted source's log is kept until then (see the reconciler). That attach names the deployment the restored data
     * was serving, whose release management then deploys (see `deployRestored`). A snapshot chosen here may still be
     * pruned before the fork first starts, which then fails to start.
     */
    async forkEnvironment(request, context) {
      const caller = callerOf(context);
      const name = slug(request.name, "name");
      // Checked before the transaction opens, so none waits on storage. A replayed request returns its first response
      // whatever this finds.
      const checked = await sourceOf(request, caller, context.signal).then(
        (source) => ({ source }),
        (error: unknown) => ({ error }),
      );
      const method = ProjectService.method.forkEnvironment;
      return idempotent({ sql, keys, caller, method, request }, async (tx) => {
        if ("error" in checked) throw checked.error;
        // Held until the fork exists, so the source can't start deleting its log before then.
        const [source] = await tx<EnvironmentRow[]>`
          select * from environments where id = ${checked.source.id} for share`;
        if (!source || source.state === EnvironmentState.DELETING) {
          throw failedPrecondition("the source environment is being deleted");
        }
        const id = newId("env");
        const [row] = await unique(
          "an environment with this name already exists in the project",
          () =>
            tx<EnvironmentRow[]>`
            insert into environments (id, project_id, name, state, sleeping_ping, hostname, forked_from_environment_id,
              forked_from_snapshot_id)
            values (${id}, ${source.project_id}, ${name}, ${EnvironmentState.PENDING}, ${SleepingPingMode.CACHE},
              ${hostnameOf(id, edge)}, ${source.id}, ${request.snapshotId})
            returning *`,
        );
        if (!row) throw new Error("environment insert returned no row");
        if (request.copySecrets) await copySecrets(tx, keys, source.id, id);
        await notify(tx, { kind: "environment", environmentId: id });
        return create(ForkEnvironmentResponseSchema, { environment: toEnvironment(row, edge) });
      });
    },

    async listSnapshots(request, context) {
      const environment = await loadEnvironment(sql, callerOf(context), request.environmentId);
      const p = page(request);
      const after = p.after === undefined ? undefined : parseSnapshotId(p.after);
      if (p.after !== undefined && !after) throw invalid("page_token is not valid");
      if (!logStore) return { snapshots: [], nextPageToken: "" };
      const snapshots = await storedSnapshots(logStore, environment.id, context.signal);
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

/**
 * The snapshots stored in the environment's log, newest first, listed until the caller goes away or `listTimeoutMs`
 * passes.
 */
async function storedSnapshots(
  logStore: LogStoreIssuer,
  environmentId: string,
  cancelled: AbortSignal,
): Promise<StoredSnapshot[]> {
  try {
    const grant = await logStore.readGrant(environmentId);
    return await listSnapshots(grant, AbortSignal.any([cancelled, AbortSignal.timeout(listTimeoutMs)]));
  } catch (error) {
    if (cancelled.aborted) throw new ConnectError("the caller went away", Code.Canceled);
    console.error("listing snapshots failed:", error);
    throw new ConnectError("the log store could not be listed; try again", Code.Unavailable);
  }
}
