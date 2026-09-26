import { create } from "@bufbuild/protobuf";
import { Code, ConnectError, type ServiceImpl } from "@connectrpc/connect";

import { notify } from "../changes.ts";
import { findRelease } from "../deployments/store.ts";
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
  state: CapacityState;
  message: string;
  machine_id: string;
  machine_addresses: string[];
  torn_down: boolean;
}

type CapacityServices = Pick<ServiceImpl<typeof EnvironmentService>, "ensureCapacity" | "releaseCapacity">;

/** Records capacity intents for the reconciler; neither call waits for the provider. */
export function capacityServices({ sql }: Deps): CapacityServices {
  return {
    async ensureCapacity(request, context) {
      const environmentId = environmentOf(context);
      const { requestId, workload, machineProfile, releaseId, appId } = request;
      if (!requestId || requestId.length > 128) throw invalid("request_id must be set, and at most 128 characters");
      if (![Workload.JVM, Workload.GATEWAY, Workload.EXEC].includes(workload)) {
        throw invalid("workload must be JVM, GATEWAY or EXEC");
      }
      required(machineProfile, "machine_profile");
      required(releaseId, "release_id");
      if ((workload === Workload.JVM) !== (appId !== "")) throw invalid("app_id is required for JVM workloads only");

      const { row } = await sql.begin(async (tx) => {
        const [environment] = await tx<{ lease: bigint; project_id: string }[]>`
          select lease, project_id from environments where id = ${environmentId} for update`;
        if (!environment) throw notFound("environment");
        fenceLease(environment.lease, request.lease);
        const [existing] = await tx<CapacityRow[]>`
          select * from capacity_requests where environment_id = ${environmentId} and request_id = ${requestId}`;
        if (existing) {
          const same =
            existing.workload === workload &&
            existing.machine_profile === machineProfile &&
            existing.release_id === releaseId &&
            existing.app_id === appId;
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
        if (workload === Workload.JVM && !release.manifest.apps.some((app) => app.id === appId)) {
          throw invalid(`release ${releaseId} has no app ${JSON.stringify(appId)}`);
        }
        const [inserted] = await tx<CapacityRow[]>`
          insert into capacity_requests
            (environment_id, request_id, workload, machine_profile, release_id, app_id, memory_mib, state)
          values (${environmentId}, ${requestId}, ${workload}, ${machineProfile}, ${releaseId}, ${appId},
            ${profile.memory_mib}, ${CapacityState.PROVISIONING})
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
        const [environment] = await tx<{ lease: bigint }[]>`
          select lease from environments where id = ${environmentId} for update`;
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
        if (released) await notify(tx, { kind: "environment", environmentId });
        const [existing] = released
          ? [released]
          : await tx<CapacityRow[]>`
              select * from capacity_requests where environment_id = ${environmentId} and request_id = ${requestId}`;
        return { row: existing };
      });
      return {
        capacity: row
          ? toCapacity(row)
          : create(CapacitySchema, { requestId: request.requestId, state: CapacityState.RELEASED }),
      };
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
