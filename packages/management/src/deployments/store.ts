import { create } from "@bufbuild/protobuf";

import { newId } from "../crypto.ts";
import type { Db } from "../db.ts";
import { DeploymentState } from "../gen/chunk/management/v1/common_pb.ts";
import {
  type Deployment,
  DeploymentSchema,
  type DeploymentTrigger,
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

const unfinished = [DeploymentState.PENDING, DeploymentState.IN_PROGRESS];

/**
 * Makes a READY release of the environment's project its desired state, superseding unfinished deployments. The
 * active deployment keeps serving until the new one activates. Call with the environment row locked.
 */
export async function createDeployment(
  db: Db,
  environment: EnvironmentRow,
  releaseId: string,
  trigger: DeploymentTrigger,
): Promise<Deployment> {
  const release = await findRelease(db, environment.project_id, releaseId);
  if (!release) throw notFound("release");
  if (release.state !== ReleaseState.READY) throw failedPrecondition("the release has not finished uploading");
  await db`
    update deployments set state = ${DeploymentState.SUPERSEDED}, update_time = now()
    where environment_id = ${environment.id} and state in ${db(unfinished)}`;
  const [row] = await db<DeploymentRow[]>`
    insert into deployments (id, environment_id, release_id, state, trigger)
    values (${newId("dep")}, ${environment.id}, ${release.id}, ${DeploymentState.PENDING}, ${trigger})
    returning *`;
  if (!row) throw new Error("deployment insert returned no row");
  return toDeployment(row);
}

/** Records that the environment serves a deployment, superseding the one it replaced. */
export async function activateDeployment(db: Db, id: string): Promise<void> {
  const [row] = await db<{ environment_id: string }[]>`
    update deployments set state = ${DeploymentState.ACTIVE}, activate_time = now(), update_time = now()
    where id = ${id} and state in ${db(unfinished)}
    returning environment_id`;
  if (!row) return;
  await db`
    update deployments set state = ${DeploymentState.SUPERSEDED}, update_time = now()
    where environment_id = ${row.environment_id} and state = ${DeploymentState.ACTIVE} and id <> ${id}`;
  await db`update environments set active_deployment_id = ${id} where id = ${row.environment_id}`;
}
