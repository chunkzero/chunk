import { createHmac, timingSafeEqual } from "node:crypto";

import { sha256 } from "../crypto.ts";
import { Workload } from "../gen/chunk/management/v1/environment_pb.ts";
import { cpusFor, type MachineSpec } from "../providers/provider.ts";
import type { CapacityRow } from "./capacity.ts";

/** How machines are built for environments. */
export interface MachineOptions {
  /** The environment image; every workload runs it and `CHUNK_WORKLOAD` says which role to take. */
  image: string;
  /** Where machines reach this service. */
  managementUrl: string;
  coreMemoryMib: number;
  /** The port core accepts extra machines on; the contract does not report one. */
  corePort: number;
}

const joinTokenPrefix = "chunkjoin.v1.";
const joinTokenLifetimeMs = 15 * 60 * 1000;
const workloadNames: Record<number, string> = {
  [Workload.JVM]: "jvm",
  [Workload.GATEWAY]: "gateway",
  [Workload.EXEC]: "exec",
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
      CHUNK_WORKLOAD: "core",
      CHUNK_ENVIRONMENT_ID: environmentId,
      CHUNK_MANAGEMENT_URL: options.managementUrl,
      CHUNK_ENVIRONMENT_TOKEN: token,
    },
    memoryMib: options.coreMemoryMib,
    cpus: cpusFor(options.coreMemoryMib),
    labels: { "chunk.environment": environmentId, "chunk.request": "core", "chunk.workload": "core" },
    volumes: [{ name: `${namePrefix(environmentId)}-data`, path: "/data" }],
    restart: true,
  };
}

/**
 * An extra machine joins core directly with a join token; it never calls this service. It is stateless and never
 * restarted by the host: the reconciler replaces one that exits, with a fresh join token.
 */
export function capacityMachineSpec(
  options: MachineOptions,
  request: CapacityRow,
  { coreAddress, environmentToken }: { coreAddress: string; environmentToken: string },
): MachineSpec {
  const workload = workloadNames[request.workload] ?? "unknown";
  return {
    name: capacityMachineName(request),
    image: options.image,
    env: {
      CHUNK_WORKLOAD: workload,
      CHUNK_ENVIRONMENT_ID: request.environment_id,
      CHUNK_CAPACITY_REQUEST_ID: request.request_id,
      CHUNK_RELEASE_ID: request.release_id,
      CHUNK_APP_ID: request.app_id,
      CHUNK_MACHINE_PROFILE: request.machine_profile,
      CHUNK_CORE_ADDRESS: coreAddress,
      CHUNK_JOIN_TOKEN: joinToken(environmentToken, {
        environment_id: request.environment_id,
        request_id: request.request_id,
        workload,
        expire_time: Math.floor((Date.now() + joinTokenLifetimeMs) / 1000),
      }),
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

export interface JoinClaims {
  environment_id: string;
  request_id: string;
  workload: string;
  /** Unix seconds. */
  expire_time: number;
}

/**
 * `chunkjoin.v1.<claims>.<mac>`: base64url JSON claims and their HMAC-SHA256, keyed by the SHA-256 of the
 * environment's token. Core holds that token, so it checks join tokens without asking this service, and a new core
 * token invalidates every earlier join token.
 */
export function joinToken(environmentToken: string, claims: JoinClaims): string {
  const body = `${joinTokenPrefix}${Buffer.from(JSON.stringify(claims)).toString("base64url")}`;
  return `${body}.${createHmac("sha256", sha256(environmentToken)).update(body).digest("base64url")}`;
}

/** The claims of a valid, unexpired join token; what core does with one a machine presents. */
export function verifyJoinToken(environmentToken: string, token: string, now = Date.now()): JoinClaims | undefined {
  const dot = token.lastIndexOf(".");
  const body = token.slice(0, dot);
  if (dot < 0 || !body.startsWith(joinTokenPrefix)) return undefined;
  const mac = createHmac("sha256", sha256(environmentToken)).update(body).digest();
  const presented = Buffer.from(token.slice(dot + 1), "base64url");
  if (presented.length !== mac.length || !timingSafeEqual(presented, mac)) return undefined;
  const claims = JSON.parse(Buffer.from(body.slice(joinTokenPrefix.length), "base64url").toString()) as JoinClaims;
  return claims.expire_time * 1000 > now ? claims : undefined;
}
