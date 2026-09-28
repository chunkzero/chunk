import { isIPv6 } from "node:net";

import { sha256 } from "../crypto.ts";
import { Workload } from "../gen/chunk/management/v1/environment_pb.ts";
import { cpusFor, type MachineSpec } from "../providers/provider.ts";
import type { CapacityRow } from "./capacity.ts";

/** How machines are built for environments. */
export interface MachineOptions {
  /** The environment image; every workload runs it and `CHUNK_SERVICES` says which services to run. */
  image: string;
  /** Where machines reach this service. */
  managementUrl: string;
  coreMemoryMib: number;
  /** The port core's network listener binds on every interface, and extra machines reach it on. */
  corePort: number;
}

const workloadNames: Record<number, string> = {
  [Workload.JVM]: "jvm",
  [Workload.GATEWAY]: "gateway",
};
/** Where each workload's machine reads the credential it presents to core. */
const credentialVariables: Record<number, string> = {
  [Workload.JVM]: "CHUNK_JVM_CREDENTIAL",
  [Workload.GATEWAY]: "CHUNK_GATEWAY_CREDENTIAL",
};

/** Machine names are unique per provider and valid hostnames, so they carry the environment ID with `_` as `-`. */
function namePrefix(environmentId: string): string {
  return `chunk-${environmentId.replaceAll("_", "-")}`;
}

export function coreMachineName(environmentId: string): string {
  return `${namePrefix(environmentId)}-core`;
}

/** Stable per request, so a replacement machine takes the name of the one it replaces. */
export function capacityMachineName(request: CapacityRow): string {
  const workload = workloadNames[request.workload] ?? "unknown";
  return `${namePrefix(request.environment_id)}-${workload}-${sha256(request.request_id).toString("hex").slice(0, 12)}`;
}

/** Core keeps its data volume and is restarted by the host. */
export function coreMachineSpec(options: MachineOptions, environmentId: string, token: string): MachineSpec {
  return {
    name: coreMachineName(environmentId),
    image: options.image,
    env: {
      CHUNK_SERVICES: "core,gateway",
      CHUNK_ENVIRONMENT_ID: environmentId,
      CHUNK_MANAGEMENT_URL: options.managementUrl,
      CHUNK_ENVIRONMENT_TOKEN: token,
      CHUNK_CORE_BIND: `[::]:${options.corePort}`,
    },
    memoryMib: options.coreMemoryMib,
    cpus: cpusFor(options.coreMemoryMib),
    labels: { "chunk.environment": environmentId, "chunk.request": "core", "chunk.workload": "core" },
    volumes: [{ name: `${namePrefix(environmentId)}-data`, path: "/data" }],
    restart: true,
  };
}

/**
 * An extra machine joins core directly with the credential core minted for it; it never calls this service. It is
 * stateless and never restarted by the host: the reconciler replaces one that exits, with the same credential.
 */
export function capacityMachineSpec(
  options: MachineOptions,
  request: CapacityRow,
  { coreHost, credential }: { coreHost: string; credential: string },
): MachineSpec {
  const workload = workloadNames[request.workload] ?? "unknown";
  const host = isIPv6(coreHost) ? `[${coreHost}]` : coreHost;
  return {
    name: capacityMachineName(request),
    image: options.image,
    env: {
      CHUNK_SERVICES: workload,
      CHUNK_ENVIRONMENT_ID: request.environment_id,
      CHUNK_CAPACITY_REQUEST_ID: request.request_id,
      CHUNK_RELEASE_ID: request.release_id,
      CHUNK_APP_ID: request.app_id,
      CHUNK_MACHINE_PROFILE: request.machine_profile,
      CHUNK_CORE_ENDPOINT: `http://${host}:${options.corePort}`,
      [credentialVariables[request.workload] ?? "CHUNK_CREDENTIAL"]: credential,
    },
    memoryMib: request.memory_mib,
    cpus: cpusFor(request.memory_mib),
    labels: {
      "chunk.environment": request.environment_id,
      "chunk.request": request.request_id,
      "chunk.workload": workload,
    },
    volumes: [],
    restart: false,
  };
}
