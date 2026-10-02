import { create } from "@bufbuild/protobuf";
import { timestampFromDate } from "@bufbuild/protobuf/wkt";
import { Code, ConnectError, type ServiceImpl } from "@connectrpc/connect";
import { and, eq, sql } from "drizzle-orm";

import type { Deps } from "../deps.ts";
import { type DeploymentService, ReleaseState, UploadTargetSchema } from "../gen/chunk/management/v1/deployments_pb.ts";
import { loadEnvironment, loadProject } from "../projects/store.ts";
import { type ReleaseManifest, verifyRelease } from "../releases/manifest.ts";
import { maxArchiveBytes, releaseKey } from "../releases/store.ts";
import { callerOf } from "../rpc/caller.ts";
import { failedPrecondition, invalid, notFound } from "../rpc/validate.ts";
import { deployments, releases as releaseRows } from "../schema.ts";
import { findRelease, toRelease } from "./store.ts";

const uploadLifetimeMs = 60 * 60 * 1000;
const releaseIdPattern = /^[A-Za-z0-9_-]{1,128}$/;

export function releaseHandlers({
  db,
  releases,
  archiveLimits,
}: Deps): Pick<ServiceImpl<typeof DeploymentService>, "uploadRelease" | "completeReleaseUpload" | "listApps"> {
  const isStored = async (key: string) => {
    const stored = await releases.read(key);
    await stored?.cancel();
    return stored !== undefined;
  };

  return {
    async uploadRelease(request, context) {
      const project = await loadProject(db, callerOf(context), request.projectId);
      if (!releaseIdPattern.test(request.releaseId)) {
        throw invalid("release_id must be 1-128 letters, digits, underscores or hyphens");
      }
      if (!/^[0-9a-f]{64}$/.test(request.archiveSha256)) throw invalid("archive_sha256 must be lowercase hex SHA-256");
      if (request.archiveSizeBytes <= 0n || request.archiveSizeBytes > maxArchiveBytes) {
        throw invalid(`archive_size_bytes must be between 1 and ${maxArchiveBytes}`);
      }
      // An unfinished upload takes the latest declared digest; a READY release never changes.
      const [declared] = await db
        .insert(releaseRows)
        .values({
          project_id: project.id,
          id: request.releaseId,
          state: ReleaseState.UPLOADING,
          archive_sha256: request.archiveSha256,
          archive_size_bytes: request.archiveSizeBytes,
        })
        .onConflictDoUpdate({
          target: [releaseRows.project_id, releaseRows.id],
          set: {
            archive_sha256: sql`excluded.archive_sha256`,
            archive_size_bytes: sql`excluded.archive_size_bytes`,
          },
          setWhere: eq(releaseRows.state, ReleaseState.UPLOADING),
        })
        .returning();
      const release = declared ?? (await findRelease(db, project.id, request.releaseId));
      if (!release) throw notFound("release");
      const key = releaseKey(project.id, release.id, release.archive_sha256);
      if (release.state === ReleaseState.READY) {
        if (
          release.archive_sha256 !== request.archiveSha256 ||
          release.archive_size_bytes !== request.archiveSizeBytes
        ) {
          throw new ConnectError("the project already holds this release with different contents", Code.AlreadyExists);
        }
        // A READY release whose archive was lost takes the same bytes again.
        if (await isStored(key)) return { release: toRelease(release) };
      }
      const expireTime = new Date(Date.now() + uploadLifetimeMs);
      const target = await releases.uploadTarget(
        key,
        { sha256: release.archive_sha256, sizeBytes: release.archive_size_bytes },
        expireTime,
      );
      return {
        release: toRelease(release),
        upload: create(UploadTargetSchema, { ...target, expireTime: timestampFromDate(expireTime) }),
      };
    },

    async completeReleaseUpload(request, context) {
      const project = await loadProject(db, callerOf(context), request.projectId);
      const release = await findRelease(db, project.id, request.releaseId);
      if (!release) throw notFound("release");
      if (release.state === ReleaseState.READY) {
        if (!(await isStored(releaseKey(project.id, release.id, release.archive_sha256)))) {
          throw failedPrecondition("the archive is missing; upload it again");
        }
        return { release: toRelease(release) };
      }

      // Verify the declaration read above, and finalize only if it is still the declaration.
      const { archive_sha256: sha256, archive_size_bytes: sizeBytes } = release;
      const stored = await releases.read(releaseKey(project.id, release.id, sha256));
      if (!stored) throw failedPrecondition("the archive has not been uploaded");
      let manifest: ReleaseManifest;
      try {
        manifest = await verifyRelease(stored, { releaseId: release.id, sha256, sizeBytes }, archiveLimits);
      } catch (error) {
        throw failedPrecondition(`the archive is not a valid release: ${(error as Error).message}`);
      }
      const [ready] = await db
        .update(releaseRows)
        .set({ state: ReleaseState.READY, manifest })
        .where(
          and(
            eq(releaseRows.project_id, project.id),
            eq(releaseRows.id, release.id),
            eq(releaseRows.state, ReleaseState.UPLOADING),
            eq(releaseRows.archive_sha256, sha256),
            eq(releaseRows.archive_size_bytes, sizeBytes),
          ),
        )
        .returning();
      if (ready) return { release: toRelease(ready) };
      const current = await findRelease(db, project.id, release.id);
      if (
        current?.state === ReleaseState.READY &&
        current.archive_sha256 === sha256 &&
        current.archive_size_bytes === sizeBytes
      ) {
        return { release: toRelease(current) };
      }
      throw new ConnectError("the release was redeclared while it was verified; retry", Code.Aborted);
    },

    async listApps(request, context) {
      const environment = await loadEnvironment(db, callerOf(context), request.environmentId);
      let releaseId = request.releaseId;
      if (!releaseId) {
        const [active] = await db
          .select({ release_id: deployments.release_id })
          .from(deployments)
          .where(eq(deployments.id, environment.active_deployment_id));
        if (!active) throw failedPrecondition("the environment has no active deployment");
        releaseId = active.release_id;
      }
      const release = await findRelease(db, environment.project_id, releaseId);
      if (!release) throw notFound("release");
      if (!release.manifest) throw failedPrecondition("the release has not finished uploading");
      return {
        apps: release.manifest.apps.map((app) => ({ id: app.id, sessions: Object.keys(app.sessions) })),
      };
    },
  };
}
