import { create } from "@bufbuild/protobuf";
import { and, eq, inArray } from "drizzle-orm";

import type { Db } from "../db.ts";
import { type AssetRevision, AssetRevisionSchema, AssetRevisionState } from "../gen/chunk/management/v1/assets_pb.ts";
import { failedPrecondition, notFound, timestamp } from "../rpc/validate.ts";
import { assetBlobs, assetRevisions } from "../schema.ts";

export type AssetRevisionRow = typeof assetRevisions.$inferSelect;

export function toAssetRevision(row: Omit<AssetRevisionRow, "manifest"> & { manifest?: Uint8Array }): AssetRevision {
  return create(AssetRevisionSchema, {
    id: row.id,
    projectId: row.project_id,
    state: row.state,
    manifest: row.manifest ?? new Uint8Array(),
    sizeBytes: row.size_bytes,
    createTime: timestamp(row.create_time),
  });
}

export async function findRevision(db: Db, projectId: string, id: string): Promise<AssetRevisionRow | undefined> {
  const [row] = await db
    .select()
    .from(assetRevisions)
    .where(and(eq(assetRevisions.project_id, projectId), eq(assetRevisions.id, id)));
  return row;
}

/** A READY revision of the project, or fails with NOT_FOUND or FAILED_PRECONDITION. */
export async function loadReadyRevision(db: Db, projectId: string, id: string): Promise<AssetRevisionRow> {
  const row = await findRevision(db, projectId, id);
  if (!row) throw notFound("asset revision");
  if (row.state !== AssetRevisionState.READY) throw failedPrecondition("the asset revision has not finished uploading");
  return row;
}

/** The project's verified blobs among `digests`, by SHA-256. */
export async function heldBlobs(
  db: Db,
  projectId: string,
  digests: string[],
): Promise<Map<string, { size_bytes: bigint; sha1: string }>> {
  if (digests.length === 0) return new Map();
  const rows = await db
    .select({ sha256: assetBlobs.sha256, size_bytes: assetBlobs.size_bytes, sha1: assetBlobs.sha1 })
    .from(assetBlobs)
    .where(and(eq(assetBlobs.project_id, projectId), inArray(assetBlobs.sha256, digests)));
  return new Map(rows.map(({ sha256, ...blob }) => [sha256, blob]));
}
