import { create } from "@bufbuild/protobuf";
import { Code, ConnectError, type ServiceImpl } from "@connectrpc/connect";

import { notify } from "../changes.ts";
import { findRelease, requireJvmImage } from "../deployments/store.ts";
import type { Deps } from "../deps.ts";
import { ReleaseState } from "../gen/chunk/management/v1/deployments_pb.ts";
import {
  type Capacity,
  CapacitySchema,
  CapacityState,
  type EnvironmentService,
  Workload,
} from "../gen/chunk/management/v1/environment_pb.ts";
import { environmentOf } from "../rpc/caller.ts";
import { failedPrecondition, invalid, notFound, required } from "../rpc/validate.ts";
import { fenceLease } from "./store.ts";

export interface CapacityRow {
  environment_id: string;
  request_id: string;
  workload: Workload;
  machine_profile: string;
  release_id: string;
  app_id: string;
  memory_mib: number;
  /** The release's Java version for JVM requests; null for gateways. */
  java_version: number | null;
  state: CapacityState;
  message: string;
  machine_id: string;
  machine_addresses: string[];
  torn_down: boolean;
  /** Whether the reconciler ever started the request's JVM machine. */
  started: boolean;
  /** Sealed under `capacityCredentialContext`. */
  credential: Uint8Array;
  /** `keys.fingerprint` of the plaintext credential. */
  credential_digest: Uint8Array;
  /** The core instance that owned the environment when the request was recorded. */
  owner_instance_id: string;
}

type CapacityServices = Pick<ServiceImpl<typeof EnvironmentService>, "ensureCapacity" | "releaseCapacity">;

/** Binds a sealed credential to its request. */
export function capacityCredentialContext(environmentId: string, requestId: string): string {
  return `capacity-credential/${environmentId}/${requestId}`;
}

/**
 * Records capacity intents for the reconciler; neither call waits for the provider.
 *
 * A release is terminal, as core's `Launcher` requires. RELEASED means the request's machine is destroyed, or was
 * created too late: it never runs, and the reconciler's sweep destroys it. The reconciler starts a machine only under a
 * lock on the request's row that sees it PROVISIONING or READY, in a transaction fenced by the leader epoch, so a
 * reconciler that lost leadership starts nothing; a JVM request's machine is started at most once. A released request
 * ID, even one released before it was ever ensured, stays released. Superseding a core instance releases its requests.
 */
export function capacityServices({ sql, keys, jvmImage }: Deps): CapacityServices {
  return {
    async ensureCapacity(request, context) {
      const environmentId = environmentOf(context);
      const { requestId, workload, machineProfile, releaseId, appId, credential } = request;
      if (!requestId || requestId.length > 128) throw invalid("request_id must be set, and at most 128 characters");
      if (![Workload.JVM, Workload.GATEWAY].includes(workload)) throw invalid("workload must be JVM or GATEWAY");
      required(machineProfile, "machine_profile");
      required(releaseId, "release_id");
      if ((workload === Workload.JVM) !== (appId !== "")) throw invalid("app_id is required for JVM workloads only");
      required(credential, "credential");
      const plaintext = new TextEncoder().encode(credential);
      const digest = keys.fingerprint(plaintext);

      const { row } = await sql.begin(async (tx) => {
        const [environment] = await tx<{ lease: bigint; project_id: string; owner_instance_id: string }[]>`
          select lease, project_id, owner_instance_id from environments where id = ${environmentId} for update`;
        if (!environment) throw notFound("environment");
        fenceLease(environment.lease, request.lease);
        const [existing] = await tx<CapacityRow[]>`
          select * from capacity_requests where environment_id = ${environmentId} and request_id = ${requestId}`;
        if (existing) {
          if (existing.workload === Workload.UNSPECIFIED) return { row: existing };
          const same =
            existing.workload === workload &&
            existing.machine_profile === machineProfile &&
            existing.release_id === releaseId &&
            existing.app_id === appId &&
            Buffer.from(existing.credential_digest).equals(digest);
          if (!same) throw new ConnectError("request_id was already used with different arguments", Code.AlreadyExists);
          return { row: existing };
        }

        const release = await findRelease(tx, environment.project_id, releaseId);
        if (!release) throw notFound("release");
        if (release.state !== ReleaseState.READY || !release.manifest) {
          throw failedPrecondition("the release has not finished uploading");
        }
        const { profiles } = release.manifest;
        const profile = Object.hasOwn(profiles, machineProfile) ? profiles[machineProfile] : undefined;
        if (!profile) throw invalid(`release ${releaseId} has no machine profile ${JSON.stringify(machineProfile)}`);
        const jvm = workload === Workload.JVM;
        if (jvm && !release.manifest.apps.some((app) => app.id === appId)) {
          throw invalid(`release ${releaseId} has no app ${JSON.stringify(appId)}`);
        }
        if (jvm) requireJvmImage(release.manifest, jvmImage);
        const sealed = await keys.cipher.seal(plaintext, capacityCredentialContext(environmentId, requestId));
        const [inserted] = await tx<CapacityRow[]>`
          insert into capacity_requests
            (environment_id, request_id, workload, machine_profile, release_id, app_id, memory_mib, java_version, state,
              credential, credential_digest, owner_instance_id)
          values (${environmentId}, ${requestId}, ${workload}, ${machineProfile}, ${releaseId}, ${appId},
            ${profile.memory_mib}, ${jvm ? (release.manifest.java_version ?? null) : null}, ${CapacityState.PROVISIONING},
            ${sealed}, ${digest}, ${environment.owner_instance_id})
          returning *`;
        await notify(tx, { kind: "environment", environmentId });
        return { row: inserted };
      });
      return { capacity: row && toCapacity(row) };
    },

    async releaseCapacity(request, context) {
      const environmentId = environmentOf(context);
      const requestId = required(request.requestId, "request_id");
      const { row } = await sql.begin(async (tx) => {
        const [environment] = await tx<{ lease: bigint; owner_instance_id: string }[]>`
          select lease, owner_instance_id from environments where id = ${environmentId} for update`;
        if (!environment) throw notFound("environment");
        fenceLease(environment.lease, request.lease);
        // RELEASED once the machine is gone: a failed request's machine may already be torn down, otherwise the
        // reconciler sets it when it removes the machine.
        const [released] = await tx<CapacityRow[]>`
          update capacity_requests
          set state = case when torn_down then ${CapacityState.RELEASED}::smallint else ${CapacityState.RELEASING}::smallint end
          where environment_id = ${environmentId} and request_id = ${requestId}
            and state in (${CapacityState.PROVISIONING}, ${CapacityState.READY}, ${CapacityState.FAILED})
          returning *`;
        if (released) {
          await notify(tx, { kind: "environment", environmentId });
          return { row: released };
        }
        const [existing] = await tx<CapacityRow[]>`
          select * from capacity_requests where environment_id = ${environmentId} and request_id = ${requestId}`;
        if (existing) return { row: existing };
        // A tombstone, so an ensure arriving after this release finds the ID released. It has no workload, which is
        // how ensures recognize it, and no machine or credential.
        const [tombstone] = await tx<CapacityRow[]>`
          insert into capacity_requests
            (environment_id, request_id, workload, machine_profile, release_id, app_id, memory_mib, state, torn_down,
              credential, credential_digest, owner_instance_id)
          values (${environmentId}, ${requestId}, ${Workload.UNSPECIFIED}, '', '', '', 0, ${CapacityState.RELEASED}, true,
            '', '', ${environment.owner_instance_id})
          returning *`;
        return { row: tombstone };
      });
      return { capacity: row && toCapacity(row) };
    },
  };
}

function toCapacity(row: CapacityRow): Capacity {
  return create(CapacitySchema, {
    requestId: row.request_id,
    state: row.state,
    machineId: row.machine_id,
    message: row.message,
  });
}
