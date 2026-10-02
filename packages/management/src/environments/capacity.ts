import { create } from "@bufbuild/protobuf";
import { Code, ConnectError, type ServiceImpl } from "@connectrpc/connect";
import { and, eq, inArray } from "drizzle-orm";

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
import { capacityRequests, environments } from "../schema.ts";
import { fenceLease, releasable, releasedState } from "./store.ts";

export type CapacityRow = typeof capacityRequests.$inferSelect;

type CapacityServices = Pick<ServiceImpl<typeof EnvironmentService>, "ensureCapacity" | "releaseCapacity">;

/** Binds a sealed credential to its request. */
export function capacityCredentialContext(environmentId: string, requestId: string): string {
  return `capacity-credential/${environmentId}/${requestId}`;
}

/**
 * Records capacity intents for the reconciler; neither call waits for the provider.
 *
 * A release is terminal, as core's `Launcher` requires. RELEASED means the request's machine is destroyed, or was
 * created too late: it never runs, and the reconciler's sweep destroys it. The reconciler starts a machine only by ID,
 * after a transaction fenced by the leader epoch saw the request PROVISIONING or READY with that machine and, for a JVM,
 * committed its one boot or the one resume after a suspension; a start still under way when the machine is torn down
 * finds its ID gone. A released request ID, even one released before it was ever ensured, stays released. Superseding a
 * core instance releases its requests.
 */
export function capacityServices({ db, keys, jvmImage }: Deps): CapacityServices {
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

      const row = await db.transaction(async (tx) => {
        const [environment] = await tx
          .select({
            lease: environments.lease,
            project_id: environments.project_id,
            owner_instance_id: environments.owner_instance_id,
          })
          .from(environments)
          .where(eq(environments.id, environmentId))
          .for("update");
        if (!environment) throw notFound("environment");
        fenceLease(environment.lease, request.lease);
        const [existing] = await tx.select().from(capacityRequests).where(thisRequest(environmentId, requestId));
        if (existing) {
          if (existing.workload === Workload.UNSPECIFIED) return existing;
          const same =
            existing.workload === workload &&
            existing.machine_profile === machineProfile &&
            existing.release_id === releaseId &&
            existing.app_id === appId &&
            Buffer.from(existing.credential_digest).equals(digest);
          if (!same) throw new ConnectError("request_id was already used with different arguments", Code.AlreadyExists);
          return existing;
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
        const [inserted] = await tx
          .insert(capacityRequests)
          .values({
            environment_id: environmentId,
            request_id: requestId,
            workload,
            machine_profile: machineProfile,
            release_id: releaseId,
            app_id: appId,
            memory_mib: profile.memory_mib,
            java_version: jvm ? (release.manifest.java_version ?? null) : null,
            state: CapacityState.PROVISIONING,
            credential: sealed,
            credential_digest: digest,
            owner_instance_id: environment.owner_instance_id,
          })
          .returning();
        await notify(tx, { kind: "environment", environmentId });
        return inserted;
      });
      return { capacity: row && toCapacity(row) };
    },

    async releaseCapacity(request, context) {
      const environmentId = environmentOf(context);
      const requestId = required(request.requestId, "request_id");
      const row = await db.transaction(async (tx) => {
        const [environment] = await tx
          .select({ lease: environments.lease, owner_instance_id: environments.owner_instance_id })
          .from(environments)
          .where(eq(environments.id, environmentId))
          .for("update");
        if (!environment) throw notFound("environment");
        fenceLease(environment.lease, request.lease);
        // RELEASED once the machine is gone: a failed request's machine may already be torn down, otherwise the
        // reconciler sets it when it removes the machine.
        const [released] = await tx
          .update(capacityRequests)
          .set({ state: releasedState })
          .where(and(thisRequest(environmentId, requestId), inArray(capacityRequests.state, releasable)))
          .returning();
        if (released) {
          await notify(tx, { kind: "environment", environmentId });
          return released;
        }
        const [existing] = await tx.select().from(capacityRequests).where(thisRequest(environmentId, requestId));
        if (existing) return existing;
        // A tombstone, so an ensure arriving after this release finds the ID released. It has no workload, which is
        // how ensures recognize it, and no machine or credential.
        const [tombstone] = await tx
          .insert(capacityRequests)
          .values({
            environment_id: environmentId,
            request_id: requestId,
            workload: Workload.UNSPECIFIED,
            machine_profile: "",
            release_id: "",
            app_id: "",
            memory_mib: 0,
            state: CapacityState.RELEASED,
            torn_down: true,
            credential: new Uint8Array(),
            credential_digest: new Uint8Array(),
            owner_instance_id: environment.owner_instance_id,
          })
          .returning();
        return tombstone;
      });
      return { capacity: row && toCapacity(row) };
    },
  };
}

function thisRequest(environmentId: string, requestId: string) {
  return and(eq(capacityRequests.environment_id, environmentId), eq(capacityRequests.request_id, requestId));
}

function toCapacity(row: CapacityRow): Capacity {
  return create(CapacitySchema, {
    requestId: row.request_id,
    state: row.state,
    machineId: row.machine_id,
    message: row.message,
  });
}
