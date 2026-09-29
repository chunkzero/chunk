import { isIP, isIPv6 } from "node:net";

import { sha256 } from "../crypto.ts";
import { Workload } from "../gen/chunk/management/v1/environment_pb.ts";
import { cpusFor, type MachineSpec } from "../providers/provider.ts";
import type { CapacityRow } from "./capacity.ts";

/** How machines are built for environments. */
export interface MachineOptions {
  /** The environment image; core and gateway machines run it, and `CHUNK_SERVICES` says which services to run. */
  image: string;
  /** The image JVM machines run, with `{java}` standing for the release's Java version. */
  jvmImage: string | undefined;
  /** Where machines reach this service. */
  managementUrl: string;
  coreMemoryMib: number;
  /** The port core's network listener binds on every interface, and extra machines reach it on. */
  corePort: number;
  /** Passed to core and gateway machines as `CHUNK_TRUSTED_EDGES`, the edges whose PROXY headers they accept. */
  trustedEdges: string | undefined;
  /** Passes `CHUNK_OFFLINE_LOGINS=1` to core and gateway machines, which then admit unauthenticated players. Insecure. */
  offlineLogins: boolean;
}

const workloadNames: Record<number, string> = {
  [Workload.JVM]: "jvm",
  [Workload.GATEWAY]: "gateway",
};

/** The runner image for a release's Java version, or undefined when there is none. */
export function jvmImage(template: string | undefined, javaVersion: number | null | undefined): string | undefined {
  if (!template || !javaVersion) return undefined;
  return template.replaceAll("{java}", String(javaVersion));
}

/** The first IP literal among core's addresses, which is what extra machines accept as core's endpoint. */
export function coreHostOf(addresses: string[]): string | undefined {
  return addresses.find((address) => isIP(address) !== 0);
}

/** Machine names are unique per provider and valid hostnames, so they carry the environment ID with `_` as `-`. */
function namePrefix(environmentId: string): string {
  return `chunk-${environmentId.replaceAll("_", "-")}`;
}

export function coreMachineName(environmentId: string): string {
  return `${namePrefix(environmentId)}-core`;
}

/** Stable per request, so a replacement machine takes the name of the one it replaces. */
export function capacityMachineName(request: Pick<CapacityRow, "environment_id" | "request_id" | "workload">): string {
  const workload = workloadNames[request.workload] ?? "unknown";
  return `${namePrefix(request.environment_id)}-${workload}-${sha256(request.request_id).toString("hex").slice(0, 12)}`;
}

/** What core and gateway machines, which admit players, get beyond their role's own variables. */
function gatewayEnv(options: MachineOptions): Record<string, string> {
  return {
    ...(options.trustedEdges ? { CHUNK_TRUSTED_EDGES: options.trustedEdges } : {}),
    ...(options.offlineLogins ? { CHUNK_OFFLINE_LOGINS: "1" } : {}),
  };
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
      ...gatewayEnv(options),
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
 * stateless and never restarted by the host: the reconciler replaces one that exits, with the same credential. A JVM
 * machine runs the runner image for its release's Java; a gateway machine runs the environment image.
 */
export function capacityMachineSpec(
  options: MachineOptions,
  request: CapacityRow,
  { coreHost, credential }: { coreHost: string; credential: string },
): MachineSpec {
  const workload = workloadNames[request.workload] ?? "unknown";
  const host = isIPv6(coreHost) ? `[${coreHost}]` : coreHost;
  const joined = {
    CHUNK_ENVIRONMENT_ID: request.environment_id,
    CHUNK_RELEASE_ID: request.release_id,
    CHUNK_APP_ID: request.app_id,
    CHUNK_MACHINE_PROFILE: request.machine_profile,
    CHUNK_CORE_ENDPOINT: `http://${host}:${options.corePort}`,
  };
  const jvm = request.workload === Workload.JVM;
  const image = jvm ? jvmImage(options.jvmImage, request.java_version) : options.image;
  if (!image) throw new Error(`no JVM image is configured for Java ${request.java_version ?? "(none)"}`);
  const env = jvm
    ? { ...joined, CHUNK_JVM_CREDENTIAL: credential }
    : {
        ...joined,
        CHUNK_SERVICES: workload,
        CHUNK_CAPACITY_REQUEST_ID: request.request_id,
        CHUNK_GATEWAY_CREDENTIAL: credential,
        ...gatewayEnv(options),
      };
  return {
    name: capacityMachineName(request),
    image,
    env,
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
