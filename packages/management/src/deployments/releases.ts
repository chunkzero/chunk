import { create } from "@bufbuild/protobuf";
import { timestampFromDate } from "@bufbuild/protobuf/wkt";
import { Code, ConnectError, type ServiceImpl } from "@connectrpc/connect";

import type { Deps } from "../deps.ts";
import { type DeploymentService, ReleaseState, UploadTargetSchema } from "../gen/chunk/management/v1/deployments_pb.ts";
import { loadEnvironment, loadProject } from "../projects/store.ts";
import { digest, readManifest } from "../releases/manifest.ts";
import { maxArchiveBytes, releaseKey } from "../releases/store.ts";
import { callerOf } from "../rpc/caller.ts";
import { failedPrecondition, invalid, notFound } from "../rpc/validate.ts";
import { findRelease, type ReleaseRow, toRelease } from "./store.ts";

const uploadLifetimeMs = 60 * 60 * 1000;
const releaseIdPattern = /^[A-Za-z0-9_-]{1,128}$/;

export function releaseHandlers({
  sql,
  releases,
}: Deps): Pick<ServiceImpl<typeof DeploymentService>, "uploadRelease" | "completeReleaseUpload" | "listApps"> {
  return {
    async uploadRelease(request, context) {
      const project = await loadProject(sql, callerOf(context), request.projectId);
      if (!releaseIdPattern.test(request.releaseId)) {
        throw invalid("release_id must be 1-128 letters, digits, underscores or hyphens");
      }
      if (!/^[0-9a-f]{64}$/.test(request.archiveSha256)) throw invalid("archive_sha256 must be lowercase hex SHA-256");
      if (request.archiveSizeBytes <= 0n || request.archiveSizeBytes > maxArchiveBytes) {
        throw invalid(`archive_size_bytes must be between 1 and ${maxArchiveBytes}`);
      }
      // An unfinished upload takes the latest declared digest; a READY release never changes.
      const [declared] = await sql<ReleaseRow[]>`
        insert into releases (project_id, id, state, archive_sha256, archive_size_bytes)
        values (${project.id}, ${request.releaseId}, ${ReleaseState.UPLOADING}, ${request.archiveSha256},
          ${request.archiveSizeBytes})
        on conflict (project_id, id) do update
          set archive_sha256 = excluded.archive_sha256, archive_size_bytes = excluded.archive_size_bytes
          where releases.state = ${ReleaseState.UPLOADING}
        returning *`;
      const release = declared ?? (await findRelease(sql, project.id, request.releaseId));
      if (!release) throw notFound("release");
      if (release.state === ReleaseState.READY) {
        if (
          release.archive_sha256 !== request.archiveSha256 ||
          release.archive_size_bytes !== request.archiveSizeBytes
        ) {
          throw new ConnectError("the project already holds this release with different contents", Code.AlreadyExists);
        }
        return { release: toRelease(release) };
      }
      const expireTime = new Date(Date.now() + uploadLifetimeMs);
      const target = await releases.uploadTarget(
        releaseKey(project.id, release.id),
        { sha256: release.archive_sha256, sizeBytes: release.archive_size_bytes },
        expireTime,
      );
      return {
        release: toRelease(release),
        upload: create(UploadTargetSchema, { ...target, expireTime: timestampFromDate(expireTime) }),
      };
    },

    async completeReleaseUpload(request, context) {
      const project = await loadProject(sql, callerOf(context), request.projectId);
      const release = await findRelease(sql, project.id, request.releaseId);
      if (!release) throw notFound("release");
      if (release.state === ReleaseState.READY) return { release: toRelease(release) };

      const archive = async () => {
        const stored = await releases.read(releaseKey(project.id, release.id));
        if (!stored) throw failedPrecondition("the archive has not been uploaded");
        return stored;
      };
      const actual = await digest(await archive());
      if (actual.sha256 !== release.archive_sha256 || actual.sizeBytes !== release.archive_size_bytes) {
        throw failedPrecondition("the uploaded archive does not match the declared size and digest");
      }
      const stored = await archive();
      let manifest: string;
      try {
        manifest = await readManifest(stored, release.id);
      } catch (error) {
        throw failedPrecondition(`the archive is not a valid release: ${(error as Error).message}`);
      }
      const [ready] = await sql<ReleaseRow[]>`
        update releases set state = ${ReleaseState.READY}, manifest = ${manifest}::text::jsonb
        where project_id = ${project.id} and id = ${release.id} and archive_sha256 = ${release.archive_sha256}
        returning *`;
      if (!ready) throw new ConnectError("the release changed while it was verified; retry", Code.Aborted);
      return { release: toRelease(ready) };
    },

    async listApps(request, context) {
      const environment = await loadEnvironment(sql, callerOf(context), request.environmentId);
      let releaseId = request.releaseId;
      if (!releaseId) {
        const [active] = await sql<{ release_id: string }[]>`
          select release_id from deployments where id = ${environment.active_deployment_id}`;
        if (!active) throw failedPrecondition("the environment has no active deployment");
        releaseId = active.release_id;
      }
      const release = await findRelease(sql, environment.project_id, releaseId);
      if (!release) throw notFound("release");
      if (!release.manifest) throw failedPrecondition("the release has not finished uploading");
      return {
        apps: release.manifest.apps.map((app) => ({ id: app.id, sessions: Object.keys(app.sessions) })),
      };
    },
  };
}
