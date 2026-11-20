import { createHash } from "node:crypto";

import { create } from "@bufbuild/protobuf";
import { timestampFromDate } from "@bufbuild/protobuf/wkt";
import { Code, ConnectError, type ServiceImpl } from "@connectrpc/connect";
import { and, desc, eq, lt } from "drizzle-orm";

import type { Db } from "../db.ts";
import type { Deps } from "../deps.ts";
import {
  AssetRevisionState,
  type AssetService,
  BlobDownloadSchema,
  BlobUploadSchema,
} from "../gen/chunk/management/v1/assets_pb.ts";
import { loadProject } from "../projects/store.ts";
import { blobKey } from "../releases/store.ts";
import { callerOf } from "../rpc/caller.ts";
import { failedPrecondition, invalid, notFound, page, pageOf, required, seqAfter } from "../rpc/validate.ts";
import { assetBlobs, assetRevisionBlobs, assetRevisions, projects } from "../schema.ts";
import { collectAssets, lockAssets, protectedUntil, uploadLifetimeMs } from "./retention.ts";
import { decodeRevision, revisionBlobs, sha256Hex } from "./revision.ts";
import { eachLimited, findRevision, heldBlobs, loadReadyRevision, toAssetRevision } from "./store.ts";

const downloadLifetimeMs = 60 * 60 * 1000;
/** How many blobs completion verifies at once. */
const verifyConcurrency = 8;
/** How many blob existence checks run at once; a revision holds at most 4,096 blobs. */
const existsConcurrency = 64;

export function assetService(deps: Deps): Partial<ServiceImpl<typeof AssetService>> {
  const { db, releases } = deps;
  /** The project's verified blobs among `digests` that the blob store still holds. */
  async function storedBlobs(conn: Db, projectId: string, digests: string[]) {
    const held = await heldBlobs(conn, projectId, digests);
    await eachLimited([...held.keys()], existsConcurrency, async (sha256) => {
      if (!(await releases.exists(blobKey(projectId, sha256)))) held.delete(sha256);
    });
    return held;
  }

  return {
    async uploadAssets(request, context) {
      const project = await loadProject(db, callerOf(context), request.projectId);
      let revision;
      try {
        revision = decodeRevision(request.manifest);
      } catch (error) {
        throw invalid((error as Error).message);
      }
      const id = sha256Hex(request.manifest);
      const blobs = revisionBlobs(revision);
      const sizeBytes = [...blobs.values()].reduce((total, blob) => total + BigInt(blob.size), 0n);
      await db.transaction(async (tx) => {
        await lockAssets(tx, project.id, "shared");
        const declared = await tx
          .insert(assetRevisions)
          .values({
            project_id: project.id,
            id,
            state: AssetRevisionState.UPLOADING,
            manifest: request.manifest,
            size_bytes: sizeBytes,
            protected_until: protectedUntil(),
          })
          .onConflictDoNothing()
          .returning({ id: assetRevisions.id });
        if (declared.length === 0) {
          // Upload URLs are issued below whether the revision is unfinished or ready, so collection keeps it as long as
          // they last.
          await tx
            .update(assetRevisions)
            .set({ protected_until: protectedUntil() })
            .where(and(eq(assetRevisions.project_id, project.id), eq(assetRevisions.id, id)));
          return;
        }
        if (blobs.size === 0) return;
        await tx
          .insert(assetRevisionBlobs)
          .values([...blobs].map(([sha256, { pack }]) => ({ project_id: project.id, revision_id: id, sha256, pack })));
      });
      const row = await findRevision(db, project.id, id);
      if (!row) throw notFound("asset revision");
      const held = await storedBlobs(db, project.id, [...blobs.keys()]);
      const expireTime = new Date(Date.now() + uploadLifetimeMs);
      const uploads = await Promise.all(
        [...blobs]
          .filter(([sha256]) => !held.has(sha256))
          .map(async ([sha256, { size }]) => {
            const target = await releases.uploadTarget(
              blobKey(project.id, sha256),
              { sha256, sizeBytes: BigInt(size) },
              expireTime,
            );
            return create(BlobUploadSchema, {
              sha256,
              upload: { ...target, expireTime: timestampFromDate(expireTime) },
            });
          }),
      );
      return { revision: toAssetRevision(row), uploads };
    },

    async completeAssetUpload(request, context) {
      const project = await loadProject(db, callerOf(context), request.projectId);
      const revisionId = required(request.revisionId, "revision_id");
      // The shared lock is taken before the revision is read and held until it is READY, so collection can't delete the
      // revision or a blob it was checked against in between, and a later declaration of the same manifest can't be
      // marked READY by an earlier completion.
      return db.transaction(async (tx) => {
        await lockAssets(tx, project.id, "shared");
        const row = await findRevision(tx, project.id, revisionId);
        if (!row) throw notFound("asset revision");
        const revision = decodeRevision(row.manifest);
        const blobs = revisionBlobs(revision);
        const held = await storedBlobs(tx, project.id, [...blobs.keys()]);
        const missing: string[] = [];
        const unverified = [...blobs].filter(([sha256]) => !held.has(sha256));
        await eachLimited(unverified, verifyConcurrency, async ([sha256, { size }]) => {
          const sizeBytes = BigInt(size);
          const sha1 = await releases.complete(blobKey(project.id, sha256), { sha256, sizeBytes }, (blob) =>
            verifyBlob(blob, sha256, sizeBytes).catch((error: unknown) => {
              throw failedPrecondition(`blob ${sha256} is invalid: ${(error as Error).message}`);
            }),
          );
          if (sha1 === undefined) {
            missing.push(sha256);
            return;
          }
          await tx
            .insert(assetBlobs)
            .values({ project_id: project.id, sha256, size_bytes: sizeBytes, sha1 })
            .onConflictDoNothing();
          held.set(sha256, { size_bytes: sizeBytes, sha1 });
        });
        if (missing.length > 0) {
          throw failedPrecondition(
            `${missing.length} of the revision's blobs are not uploaded, such as ${missing.sort()[0]}`,
          );
        }
        for (const [sha256, { size }] of blobs) {
          if (held.get(sha256)?.size_bytes !== BigInt(size)) {
            throw failedPrecondition(`blob ${sha256} is not ${size} bytes, as the revision declares`);
          }
        }
        for (const [name, pack] of revision.packs) {
          if (held.get(pack.sha256)?.sha1 !== pack.sha1) {
            throw failedPrecondition(`pack ${name}'s SHA-1 does not match its bytes`);
          }
        }
        if (row.state === AssetRevisionState.READY) return { revision: toAssetRevision(row) };
        const [ready] = await tx
          .update(assetRevisions)
          .set({ state: AssetRevisionState.READY })
          .where(and(eq(assetRevisions.project_id, project.id), eq(assetRevisions.id, row.id)))
          .returning();
        if (!ready) throw notFound("asset revision");
        return { revision: toAssetRevision(ready) };
      });
    },

    async getAssetRevision(request, context) {
      const project = await loadProject(db, callerOf(context), request.projectId);
      const id = request.revisionId || project.asset_head_id;
      if (!id) return {};
      const row = await loadReadyRevision(db, project.id, id);
      if (!request.downloads) return { revision: toAssetRevision(row) };
      const expireTime = new Date(Date.now() + downloadLifetimeMs);
      const digests = [...revisionBlobs(decodeRevision(row.manifest)).keys()];
      const downloads = await Promise.all(
        digests.map(async (sha256) =>
          create(BlobDownloadSchema, {
            sha256,
            url: await releases.downloadUrl(blobKey(project.id, sha256), expireTime, "clients"),
          }),
        ),
      );
      return { revision: toAssetRevision(row), downloads };
    },

    async listAssetRevisions(request, context) {
      const project = await loadProject(db, callerOf(context), request.projectId);
      const p = page(request);
      const before = seqAfter(p);
      const rows = await db
        .select({
          seq: assetRevisions.seq,
          project_id: assetRevisions.project_id,
          id: assetRevisions.id,
          state: assetRevisions.state,
          size_bytes: assetRevisions.size_bytes,
          create_time: assetRevisions.create_time,
        })
        .from(assetRevisions)
        .where(
          and(
            eq(assetRevisions.project_id, project.id),
            eq(assetRevisions.state, AssetRevisionState.READY),
            before === undefined ? undefined : lt(assetRevisions.seq, before),
          ),
        )
        .orderBy(desc(assetRevisions.seq))
        .limit(p.size + 1);
      const { items, nextPageToken } = pageOf(rows, p, (row) => row.seq.toString());
      return { revisions: items.map(toAssetRevision), headId: project.asset_head_id, nextPageToken };
    },

    async setAssetHead(request, context) {
      const project = await loadProject(db, callerOf(context), request.projectId);
      const revisionId = required(request.revisionId, "revision_id");
      // Shared with uploads, so a collection never deletes the revision between checking it and moving the head to it.
      await db.transaction(async (tx) => {
        await lockAssets(tx, project.id, "shared");
        const revision = await loadReadyRevision(tx, project.id, revisionId);
        const [moved] = await tx
          .update(projects)
          .set({ asset_head_id: revision.id })
          .where(and(eq(projects.id, project.id), eq(projects.asset_head_id, request.expectedHeadId)))
          .returning({ id: projects.id });
        if (!moved) {
          const [current] = await tx
            .select({ head: projects.asset_head_id })
            .from(projects)
            .where(eq(projects.id, project.id));
          throw new ConnectError(`the asset head moved to ${current?.head || "nothing"}`, Code.Aborted);
        }
      });
      // Each push moves the head, so the revisions it leaves behind are collected here.
      await collectAssets(deps, project.id).catch((error: unknown) => {
        console.error(`collecting project ${project.id}'s unused asset revisions failed:`, error);
      });
      return { previousHeadId: request.expectedHeadId };
    },
  };
}

/** Checks a blob's size and SHA-256 in one pass, and returns its SHA-1. */
async function verifyBlob(blob: ReadableStream<Uint8Array>, sha256: string, sizeBytes: bigint): Promise<string> {
  const [sha256Hash, sha1Hash] = [createHash("sha256"), createHash("sha1")];
  let size = 0n;
  for await (const chunk of blob) {
    size += BigInt(chunk.byteLength);
    if (size > sizeBytes) throw new Error("it is larger than declared");
    sha256Hash.update(chunk);
    sha1Hash.update(chunk);
  }
  if (size !== sizeBytes || sha256Hash.digest("hex") !== sha256) {
    throw new Error("it does not match the declared size and digest");
  }
  return sha1Hash.digest("hex");
}
