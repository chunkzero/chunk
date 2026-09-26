import { create } from "@bufbuild/protobuf";

import type { Db } from "../db.ts";
import type { Deps } from "../deps.ts";
import { DeploymentState } from "../gen/chunk/management/v1/common_pb.ts";
import {
  type AttachResponse,
  AttachResponseSchema,
  ObjectStoreSchema,
  ReleaseArtifactSchema,
} from "../gen/chunk/management/v1/environment_pb.ts";
import { EnvironmentState } from "../gen/chunk/management/v1/projects_pb.ts";
import { releaseKey } from "../releases/store.ts";
import { notFound } from "../rpc/validate.ts";
import { secretContext } from "../secrets/service.ts";

const artifactUrlLifetimeMs = 60 * 60 * 1000;
const served = [DeploymentState.PENDING, DeploymentState.IN_PROGRESS, DeploymentState.ACTIVE];

interface DesiredDeployment {
  id: string;
  release_id: string;
  project_id: string;
  archive_sha256: string;
  archive_size_bytes: bigint;
}

/** The deployment an environment should serve: its newest one that neither failed nor was superseded. */
export async function desiredDeployment(db: Db, environmentId: string): Promise<DesiredDeployment | undefined> {
  const [row] = await db<DesiredDeployment[]>`
    select d.id, d.release_id, r.project_id, r.archive_sha256, r.archive_size_bytes
    from deployments d
    join environments e on e.id = d.environment_id
    join releases r on r.project_id = e.project_id and r.id = d.release_id
    where d.environment_id = ${environmentId} and d.state in ${db(served)}
    order by d.seq desc
    limit 1`;
  return row;
}

/** The environment's complete desired state, read from one snapshot, and the lease of its current owner. */
export async function desiredState(
  { sql, keys, releases, logStore }: Deps,
  environmentId: string,
): Promise<{ message: AttachResponse; lease: bigint }> {
  const { snapshot } = await sql.begin("isolation level repeatable read read only", async (tx) => {
    const [environment] = await tx<{ project_id: string; revision: bigint; lease: bigint; state: EnvironmentState }[]>`
      select project_id, revision, lease, state from environments where id = ${environmentId}`;
    if (!environment || environment.state === EnvironmentState.DELETING) throw notFound("environment");
    const deployment = await desiredDeployment(tx, environmentId);
    const secrets = await tx<{ name: string; version: bigint; ciphertext: Uint8Array }[]>`
      select name, version, ciphertext from secrets
      where environment_id = ${environmentId} and ciphertext is not null
      order by name`;
    return { snapshot: { environment, deployment, secrets } };
  });
  const { environment, deployment, secrets } = snapshot;

  const message = create(AttachResponseSchema, {
    revision: environment.revision,
    environmentId,
    projectId: environment.project_id,
    secrets: await Promise.all(
      secrets.map(async ({ name, version, ciphertext }) => ({
        name,
        version,
        value: await keys.cipher.open(ciphertext, secretContext(environmentId, name)),
      })),
    ),
  });
  if (deployment) {
    const key = releaseKey(deployment.project_id, deployment.release_id, deployment.archive_sha256);
    message.deploymentId = deployment.id;
    message.release = create(ReleaseArtifactSchema, {
      releaseId: deployment.release_id,
      url: await releases.downloadUrl(key, new Date(Date.now() + artifactUrlLifetimeMs)),
      sha256: deployment.archive_sha256,
      sizeBytes: deployment.archive_size_bytes,
    });
  }
  if (logStore) {
    message.logStore = create(ObjectStoreSchema, {
      endpoint: logStore.endpoint,
      region: logStore.region,
      bucket: logStore.bucket,
      prefix: `${logStore.prefix}${environmentId}/`,
      accessKeyId: logStore.accessKeyId,
      secretAccessKey: logStore.secretAccessKey,
    });
  }
  return { message, lease: environment.lease };
}
