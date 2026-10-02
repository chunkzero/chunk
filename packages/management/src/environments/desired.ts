import { create } from "@bufbuild/protobuf";
import { and, desc, eq, inArray, isNotNull, sql } from "drizzle-orm";

import type { Db } from "../db.ts";
import type { Deps } from "../deps.ts";
import { DeploymentState } from "../gen/chunk/management/v1/common_pb.ts";
import {
  type AttachResponse,
  AttachResponseSchema,
  ObjectStoreSchema,
  ReleaseArtifactSchema,
  RestoreSchema,
} from "../gen/chunk/management/v1/environment_pb.ts";
import { EnvironmentState } from "../gen/chunk/management/v1/projects_pb.ts";
import { releaseKey } from "../releases/store.ts";
import { notFound, timestamp } from "../rpc/validate.ts";
import { deployments, environments, releases, secrets } from "../schema.ts";
import { secretContext } from "../secrets/service.ts";

const artifactUrlLifetimeMs = 60 * 60 * 1000;
const served = [DeploymentState.PENDING, DeploymentState.IN_PROGRESS, DeploymentState.ACTIVE];

/** The deployment an environment should serve: its newest one that neither failed nor was superseded. */
export async function desiredDeployment(db: Db, environmentId: string) {
  const [row] = await db
    .select({
      id: deployments.id,
      release_id: deployments.release_id,
      project_id: releases.project_id,
      archive_sha256: releases.archive_sha256,
      archive_size_bytes: releases.archive_size_bytes,
      stop_previous: deployments.stop_previous,
    })
    .from(deployments)
    .innerJoin(environments, eq(environments.id, deployments.environment_id))
    .innerJoin(releases, and(eq(releases.project_id, environments.project_id), eq(releases.id, deployments.release_id)))
    .where(and(eq(deployments.environment_id, environmentId), inArray(deployments.state, served)))
    .orderBy(desc(deployments.seq))
    .limit(1);
  return row;
}

/** The environment's complete desired state, read from one snapshot, and the lease of its current owner. */
export async function desiredState(
  { db, keys, releases: releaseStore, logStore }: Deps,
  environmentId: string,
): Promise<{ message: AttachResponse; lease: bigint }> {
  const snapshot = await db.transaction(
    async (tx) => {
      const [environment] = await tx
        .select({
          project_id: environments.project_id,
          name: environments.name,
          revision: environments.revision,
          lease: environments.lease,
          state: environments.state,
          drain_max_age_seconds: environments.drain_max_age_seconds,
          drain_deadline_seconds: environments.drain_deadline_seconds,
          forked_from_environment_id: environments.forked_from_environment_id,
          forked_from_snapshot_id: environments.forked_from_snapshot_id,
          epoch: environments.epoch,
        })
        .from(environments)
        .where(eq(environments.id, environmentId));
      if (!environment || environment.state === EnvironmentState.DELETING) throw notFound("environment");
      const deployment = await desiredDeployment(tx, environmentId);
      const stored = await tx
        .select({ name: secrets.name, version: secrets.version, ciphertext: sql<Uint8Array>`${secrets.ciphertext}` })
        .from(secrets)
        .where(and(eq(secrets.environment_id, environmentId), isNotNull(secrets.ciphertext)))
        .orderBy(secrets.name);
      return { environment, deployment, secrets: stored };
    },
    { isolationLevel: "repeatable read", accessMode: "read only" },
  );
  const { environment, deployment } = snapshot;

  const message = create(AttachResponseSchema, {
    revision: environment.revision,
    environmentId,
    environmentName: environment.name,
    projectId: environment.project_id,
    drain: { maxAgeSeconds: environment.drain_max_age_seconds, deadlineSeconds: environment.drain_deadline_seconds },
    secrets: await Promise.all(
      snapshot.secrets.map(async ({ name, version, ciphertext }) => ({
        name,
        version,
        value: await keys.cipher.open(ciphertext, secretContext(environmentId, name)),
      })),
    ),
  });
  if (deployment) {
    const key = releaseKey(deployment.project_id, deployment.release_id, deployment.archive_sha256);
    message.deploymentId = deployment.id;
    message.stopPrevious = deployment.stop_previous;
    message.release = create(ReleaseArtifactSchema, {
      releaseId: deployment.release_id,
      url: await releaseStore.downloadUrl(key, new Date(Date.now() + artifactUrlLifetimeMs)),
      sha256: deployment.archive_sha256,
      sizeBytes: deployment.archive_size_bytes,
    });
  }
  if (logStore) {
    const { expireTime, ...grant } = await logStore.grant(environmentId);
    message.logStore = create(ObjectStoreSchema, { ...grant, expireTime: timestamp(expireTime) });
    // Core attaches only once its log is open, which for a fork means its own log holds a snapshot: from then on it
    // restores from there, so the source may go.
    const source = environment.forked_from_environment_id;
    if (source && environment.epoch === 0n) {
      const { expireTime: sourceExpireTime, ...sourceGrant } = await logStore.readGrant(source);
      message.restore = create(RestoreSchema, {
        source: { ...sourceGrant, expireTime: timestamp(sourceExpireTime) },
        snapshotId: environment.forked_from_snapshot_id,
        sourceEnvironmentId: source,
      });
    }
  }
  return { message, lease: environment.lease };
}
