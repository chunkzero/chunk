import { create } from "@bufbuild/protobuf";
import { and, eq, inArray, ne, sql } from "drizzle-orm";

import { newId } from "../crypto.ts";
import { type Db, fetchRows } from "../db.ts";
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
import { deployments, environments, releases } from "../schema.ts";

export type ReleaseRow = typeof releases.$inferSelect;
export type DeploymentRow = typeof deployments.$inferSelect;

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
  const [row] = await db
    .select()
    .from(releases)
    .where(and(eq(releases.project_id, projectId), eq(releases.id, id)));
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
  await db
    .update(deployments)
    .set({ state: DeploymentState.SUPERSEDED, update_time: sql`now()` })
    .where(and(eq(deployments.environment_id, environment.id), inArray(deployments.state, unfinished)));
  const [row] = await db
    .insert(deployments)
    .values({
      id: newId("dep"),
      environment_id: environment.id,
      release_id: release.id,
      state: DeploymentState.PENDING,
      trigger,
      stop_previous: stopPrevious,
    })
    .returning();
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
  const [fork] = await db
    .select()
    .from(environments)
    .where(and(eq(environments.id, forkId), ne(environments.forked_from_environment_id, "")));
  if (!fork) return;
  const [deployed] = await db
    .select({ one: sql`1` })
    .from(deployments)
    .where(eq(deployments.environment_id, forkId))
    .limit(1);
  if (deployed) return;
  const [restored] = await db
    .select({ release_id: deployments.release_id })
    .from(deployments)
    .innerJoin(environments, eq(environments.id, deployments.environment_id))
    .where(and(eq(deployments.id, deploymentId), eq(environments.project_id, fork.project_id)));
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
  const [row] = await fetchRows<{ environment_id: string }>(
    db,
    sql`
    update deployments d set state = ${DeploymentState.ACTIVE}, activate_time = now(), update_time = now()
    where d.id = ${id} and (
      d.state in ${unfinished}
      or (d.state = ${DeploymentState.SUPERSEDED} and d.seq > coalesce((
        select max(a.seq) from deployments a
        where a.environment_id = d.environment_id and a.state = ${DeploymentState.ACTIVE}
      ), 0))
    )
    returning environment_id`,
  );
  if (!row) return;
  await db
    .update(deployments)
    .set({ state: DeploymentState.SUPERSEDED, update_time: sql`now()` })
    .where(
      and(
        eq(deployments.environment_id, row.environment_id),
        eq(deployments.state, DeploymentState.ACTIVE),
        ne(deployments.id, id),
      ),
    );
  await db.update(environments).set({ active_deployment_id: id }).where(eq(environments.id, row.environment_id));
}

/** Records that the environment could not start a deployment; the previous one keeps serving. */
export async function failDeployment(db: Db, id: string, message: string): Promise<void> {
  const [row] = await db
    .update(deployments)
    .set({ state: DeploymentState.FAILED, message, update_time: sql`now()` })
    .where(and(eq(deployments.id, id), inArray(deployments.state, unfinished)))
    .returning({ environment_id: deployments.environment_id });
  if (row) await advanceRevision(db, row.environment_id);
}

/** Records that the environment started a deployment. */
export async function progressDeployment(db: Db, id: string): Promise<void> {
  await db
    .update(deployments)
    .set({ state: DeploymentState.IN_PROGRESS, update_time: sql`now()` })
    .where(and(eq(deployments.id, id), eq(deployments.state, DeploymentState.PENDING)));
}
