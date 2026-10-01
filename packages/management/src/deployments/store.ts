import { create } from "@bufbuild/protobuf";

import { newId } from "../crypto.ts";
import type { Db } from "../db.ts";
import { jvmImage } from "../environments/machines.ts";
import { advanceRevision } from "../environments/store.ts";
import { DeploymentState } from "../gen/chunk/management/v1/common_pb.ts";
import {
  type Deployment,
  DeploymentSchema,
  DeploymentTrigger,
  type Release,
  ReleaseSchema,
  ReleaseState,
} from "../gen/chunk/management/v1/deployments_pb.ts";
import type { EnvironmentRow } from "../projects/store.ts";
import type { ReleaseManifest } from "../releases/manifest.ts";
import { failedPrecondition, notFound, timestamp } from "../rpc/validate.ts";

export interface ReleaseRow {
  project_id: string;
  id: string;
  state: ReleaseState;
  archive_sha256: string;
  archive_size_bytes: bigint;
  manifest: ReleaseManifest | null;
  create_time: Date;
}

export interface DeploymentRow {
  seq: bigint;
  id: string;
  environment_id: string;
  release_id: string;
  state: DeploymentState;
  trigger: DeploymentTrigger;
  message: string;
  create_time: Date;
  update_time: Date;
  activate_time: Date | null;
}

export function toRelease(row: ReleaseRow): Release {
  return create(ReleaseSchema, {
    id: row.id,
    projectId: row.project_id,
    state: row.state,
    archiveSha256: row.archive_sha256,
    archiveSizeBytes: row.archive_size_bytes,
    createTime: timestamp(row.create_time),
  });
}

export function toDeployment(row: DeploymentRow): Deployment {
  return create(DeploymentSchema, {
    id: row.id,
    environmentId: row.environment_id,
    releaseId: row.release_id,
    state: row.state,
    trigger: row.trigger,
    message: row.message,
    createTime: timestamp(row.create_time),
    updateTime: timestamp(row.update_time),
  });
}

export async function findRelease(db: Db, projectId: string, id: string): Promise<ReleaseRow | undefined> {
  const [row] = await db<ReleaseRow[]>`select * from releases where project_id = ${projectId} and id = ${id}`;
  return row;
}

/** Refuses a release whose Java version no JVM image runs. */
export function requireJvmImage(manifest: ReleaseManifest, template: string | undefined): void {
  if (jvmImage(template, manifest.java_version)) return;
  throw failedPrecondition(
    `no JVM image is configured for release ${manifest.id}'s Java version (${manifest.java_version ?? "none"})`,
  );
}

const unfinished = [DeploymentState.PENDING, DeploymentState.IN_PROGRESS];

/**
 * Makes a READY release of the environment's project its desired state, superseding unfinished deployments. The
 * active deployment keeps serving until the new one activates. A release no JVM image runs is refused. Call with the
 * environment row locked.
 */
export async function createDeployment(
  db: Db,
  environment: EnvironmentRow,
  releaseId: string,
  trigger: DeploymentTrigger,
  jvmImageTemplate: string | undefined,
  stopPrevious = false,
): Promise<Deployment> {
  const release = await findRelease(db, environment.project_id, releaseId);
  if (!release) throw notFound("release");
  if (release.state !== ReleaseState.READY || !release.manifest) {
    throw failedPrecondition("the release has not finished uploading");
  }
  requireJvmImage(release.manifest, jvmImageTemplate);
  await db`
    update deployments set state = ${DeploymentState.SUPERSEDED}, update_time = now()
    where environment_id = ${environment.id} and state in ${db(unfinished)}`;
  const [row] = await db<DeploymentRow[]>`
    insert into deployments (id, environment_id, release_id, state, trigger, stop_previous)
    values (${newId("dep")}, ${environment.id}, ${release.id}, ${DeploymentState.PENDING}, ${trigger}, ${stopPrevious})
    returning *`;
  if (!row) throw new Error("deployment insert returned no row");
  await advanceRevision(db, environment.id);
  return toDeployment(row);
}

/**
 * Deploys to a fork the release of `deploymentId`, the newest deployment resident in the database it restored, unless
 * the fork has a deployment already. That deployment belongs to an environment of the fork's project; when it is gone,
 * or no JVM image runs its release, the fork stays without a deployment until one is deployed to it.
 */
export async function deployRestored(
  db: Db,
  forkId: string,
  deploymentId: string,
  jvmImageTemplate: string | undefined,
): Promise<void> {
  const [fork] = await db<EnvironmentRow[]>`
    select * from environments where id = ${forkId} and forked_from_environment_id <> ''`;
  if (!fork) return;
  const [deployed] = await db`select 1 from deployments where environment_id = ${forkId} limit 1`;
  if (deployed) return;
  const [restored] = await db<{ release_id: string }[]>`
    select d.release_id from deployments d join environments e on e.id = d.environment_id
    where d.id = ${deploymentId} and e.project_id = ${fork.project_id}`;
  const release = restored && (await findRelease(db, fork.project_id, restored.release_id));
  const java = release?.manifest?.java_version;
  if (release?.state !== ReleaseState.READY || !release.manifest || !jvmImage(jvmImageTemplate, java)) {
    console.warn(
      `fork ${forkId} stays without a deployment: its restored deployment ${deploymentId} can't be deployed`,
    );
    return;
  }
  await createDeployment(db, fork, release.id, DeploymentTrigger.FORK, jvmImageTemplate);
}

/**
 * Records that the environment serves a deployment, superseding the one it replaced. A superseded deployment the
 * environment activated anyway counts when it is newer than the active one; newer unfinished deployments stay desired.
 */
export async function activateDeployment(db: Db, id: string): Promise<void> {
  const [row] = await db<{ environment_id: string }[]>`
    update deployments d set state = ${DeploymentState.ACTIVE}, activate_time = now(), update_time = now()
    where d.id = ${id} and (
      d.state in ${db(unfinished)}
      or (d.state = ${DeploymentState.SUPERSEDED} and d.seq > coalesce((
        select max(a.seq) from deployments a
        where a.environment_id = d.environment_id and a.state = ${DeploymentState.ACTIVE}
      ), 0))
    )
    returning environment_id`;
  if (!row) return;
  await db`
    update deployments set state = ${DeploymentState.SUPERSEDED}, update_time = now()
    where environment_id = ${row.environment_id} and state = ${DeploymentState.ACTIVE} and id <> ${id}`;
  await db`update environments set active_deployment_id = ${id} where id = ${row.environment_id}`;
}

/** Records that the environment could not start a deployment; the previous one keeps serving. */
export async function failDeployment(db: Db, id: string, message: string): Promise<void> {
  const [row] = await db<{ environment_id: string }[]>`
    update deployments set state = ${DeploymentState.FAILED}, message = ${message}, update_time = now()
    where id = ${id} and state in ${db(unfinished)}
    returning environment_id`;
  if (row) await advanceRevision(db, row.environment_id);
}

/** Records that the environment started a deployment. */
export async function progressDeployment(db: Db, id: string): Promise<void> {
  await db`
    update deployments set state = ${DeploymentState.IN_PROGRESS}, update_time = now()
    where id = ${id} and state = ${DeploymentState.PENDING}`;
}
