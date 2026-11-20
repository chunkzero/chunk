import { and, eq, inArray, notExists, sql } from "drizzle-orm";

import { type Db, fetchRows } from "../db.ts";
import type { Deps } from "../deps.ts";
import { AssetRevisionState } from "../gen/chunk/management/v1/assets_pb.ts";
import { DeploymentState } from "../gen/chunk/management/v1/common_pb.ts";
import { blobKey } from "../releases/store.ts";
import { assetBlobs, assetDeletions, assetRevisionBlobs } from "../schema.ts";
import { eachLimited } from "./store.ts";

/** How many of a project's newest ready revisions are kept, deployed or not. */
export const keptRecentRevisions = 10;
/** How many of each environment's newest deployments keep the revisions they pin, so they can be rolled back to. */
export const keptRecentDeployments = 10;
/** How long a revision is kept after an upload of it was declared: as long as its upload URLs last. */
export const uploadLifetimeMs = 60 * 60 * 1000;
/** The time until which an upload declared now protects its revision from collection. */
export const protectedUntil = () => sql`now() + make_interval(secs => ${uploadLifetimeMs / 1000})`;
/** How many blobs are removed from the release store at once. */
const deleteConcurrency = 16;

/**
 * Serializes collection, which takes the lock `exclusive`ly, with what makes a project's revisions or blobs used:
 * declaring a revision's blobs and pinning a revision in a deployment. Held until the transaction `tx` ends.
 */
export async function lockAssets(tx: Db, projectId: string, mode: "shared" | "exclusive"): Promise<void> {
  const key = `assets/${projectId}`;
  await tx.execute(
    mode === "shared"
      ? sql`select pg_advisory_xact_lock_shared(hashtext(${key}))`
      : sql`select pg_advisory_xact_lock(hashtext(${key}))`,
  );
}

/**
 * Deletes the project's revisions that nothing keeps, and then the blobs no remaining revision holds, from the database
 * and the release store. A revision is kept while it is the project's head, one of its `keptRecentRevisions` newest
 * ready revisions, protected by an upload declared within `uploadLifetimeMs`, pinned by the active, an unfinished or one
 * of the `keptRecentDeployments` newest deployments of any of its environments, or pinned by a superseded deployment
 * that is still draining: until the later of the deadline its environment had when it was superseded and the one it has
 * now, so shortening the deadline never removes what core may still serve. Returns how many revisions it deleted and how many blobs it removed from the release store.
 *
 * The database rows are deleted first, recording the blobs' keys in `asset_deletions`; a later transaction deletes the
 * bytes of each key no row has claimed again. A store delete that fails leaves its key recorded, so the next
 * collection retries it.
 */
export async function collectAssets(
  { db, releases }: Deps,
  projectId: string,
): Promise<{ revisions: number; blobs: number }> {
  const revisions = await db.transaction((tx) => deleteUnusedRows(tx, projectId));
  const blobs = await db.transaction(async (tx) => {
    await lockAssets(tx, projectId, "exclusive");
    const pending = await fetchRows<{ sha256: string; referenced: boolean }>(
      tx,
      sql`
      select d.sha256,
        exists (select 1 from asset_blobs b where b.project_id = d.project_id and b.sha256 = d.sha256)
          or exists (select 1 from asset_revision_blobs b where b.project_id = d.project_id and b.sha256 = d.sha256)
          as referenced
      from asset_deletions d where d.project_id = ${projectId}`,
    );
    const done = pending.filter(({ referenced }) => referenced).map(({ sha256 }) => sha256);
    const unreferenced = pending.filter(({ referenced }) => !referenced).map(({ sha256 }) => sha256);
    const deleted: string[] = [];
    await eachLimited(unreferenced, deleteConcurrency, (sha256) =>
      releases.delete(blobKey(projectId, sha256)).then(
        () => void deleted.push(sha256),
        (error: unknown) => console.error(`deleting asset blob ${sha256} of project ${projectId} failed:`, error),
      ),
    );
    const settled = [...done, ...deleted];
    if (settled.length > 0) {
      await tx
        .delete(assetDeletions)
        .where(and(eq(assetDeletions.project_id, projectId), inArray(assetDeletions.sha256, settled)));
    }
    return deleted.length;
  });
  return { revisions, blobs };
}

/** Deletes the revisions nothing keeps and then the blobs none holds, recording the blobs to delete from the store. */
async function deleteUnusedRows(tx: Db, projectId: string): Promise<number> {
  await lockAssets(tx, projectId, "exclusive");
  const served = [DeploymentState.PENDING, DeploymentState.IN_PROGRESS, DeploymentState.ACTIVE];
  // Every CTE reads the snapshot from before the deletion, so `blobs` holds the deleted revisions' blobs.
  const [deleted] = await fetchRows<{ revisions: number; blobs: string[] | null }>(
    tx,
    sql`
    with deployed as (
      select d.asset_revision_id, d.state, d.id = e.active_deployment_id as active,
        d.state = ${DeploymentState.SUPERSEDED}
          and now() < greatest(d.drain_until, d.update_time + make_interval(secs => e.drain_deadline_seconds)) as draining,
        row_number() over (partition by d.environment_id order by d.seq desc) as recency
      from deployments d join environments e on e.id = d.environment_id
      where e.project_id = ${projectId}
    ),
    kept as (
      select asset_head_id as id from projects where id = ${projectId}
      union
      select id from (
        select id from asset_revisions where project_id = ${projectId} and state = ${AssetRevisionState.READY}
        order by seq desc limit ${keptRecentRevisions}
      ) recent
      union
      select id from asset_revisions where project_id = ${projectId} and protected_until > now()
      union
      select asset_revision_id from deployed
      where active or draining or recency <= ${keptRecentDeployments} or state in (${sql.join(served, sql`, `)})
    ),
    doomed as (
      select r.id from asset_revisions r
      where r.project_id = ${projectId} and r.id not in (select id from kept)
    ),
    blobs as (
      select distinct b.sha256 from asset_revision_blobs b join doomed on doomed.id = b.revision_id
      where b.project_id = ${projectId}
    ),
    gone as (
      delete from asset_revisions r using doomed where r.project_id = ${projectId} and r.id = doomed.id
      returning r.id
    )
    select (select count(*) from gone)::int as revisions, (select array_agg(sha256) from blobs) as blobs`,
  );
  const unverified = await tx
    .delete(assetBlobs)
    .where(
      and(
        eq(assetBlobs.project_id, projectId),
        notExists(
          tx
            .select({ one: sql`1` })
            .from(assetRevisionBlobs)
            .where(and(eq(assetRevisionBlobs.project_id, projectId), eq(assetRevisionBlobs.sha256, assetBlobs.sha256))),
        ),
      ),
    )
    .returning({ sha256: assetBlobs.sha256 });
  const orphans = new Set([...(deleted?.blobs ?? []), ...unverified.map((blob) => blob.sha256)]);
  if (orphans.size > 0) {
    const held = await tx
      .selectDistinct({ sha256: assetRevisionBlobs.sha256 })
      .from(assetRevisionBlobs)
      .where(eq(assetRevisionBlobs.project_id, projectId));
    for (const { sha256 } of held) orphans.delete(sha256);
  }
  if (orphans.size > 0) {
    await tx
      .insert(assetDeletions)
      .values([...orphans].map((sha256) => ({ project_id: projectId, sha256 })))
      .onConflictDoNothing();
  }
  return deleted?.revisions ?? 0;
}
