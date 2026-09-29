import type { ArchiveLimits } from "./releases/archive.ts";

export interface Config {
  databaseUrl: string;
  /** 32 bytes; encrypts secrets and signs upload URLs. */
  secretKey: Uint8Array;
  /** Bootstraps the operator's first API token. */
  operatorToken: string | undefined;
  /** How clients reach this service; used in upload and login URLs. */
  publicUrl: string;
  host: string;
  port: number;
  /** Local release archives live under here. */
  dataDir: string;
  archiveLimits: ArchiveLimits;
  /** Where players reach environments; unset leaves environments without hostnames. */
  edge: Edge | undefined;
  /** Bootstraps the token edges call EdgeService with. */
  edgeToken: string | undefined;
  /** Where environments replicate their logs; unset turns replication off. */
  logStore: LogStore | undefined;
  /** How environments get machines; unset leaves every environment unprovisioned. */
  machines: Machines | undefined;
  /** The dashboard's static build; unset serves no dashboard. */
  dashboardDir: string | undefined;
}

export interface Machines {
  /** A Docker-compatible engine socket, as unix://<path>. */
  dockerHost: string;
  /** The container network machines share. */
  network: string;
  image: string;
  /** The image JVM machines run, with `{java}` standing for the release's Java version; unset, no JVM can run. */
  jvmImage: string | undefined;
  managementUrl: string;
  coreMemoryMib: number;
  corePort: number;
  /** The edges core and gateway machines accept PROXY headers from, as comma-separated IPs or CIDRs. */
  trustedEdges: string | undefined;
  /** Lets core and gateway machines admit unauthenticated players under any name. Insecure; for smoke tests only. */
  offlineLogins: boolean;
}

/** An S3-compatible bucket; each environment replicates below `<prefix><environment ID>/`. */
export interface LogStore {
  endpoint: string;
  region: string;
  bucket: string;
  prefix: string;
  /** The operator's credentials, which environments get only with `sharedCredentials`. */
  accessKeyId: string;
  secretAccessKey: string;
  /** Hands every environment the operator's credentials, trusting each with every other environment's logs. */
  sharedCredentials: boolean;
  /** Where environments' prefix-limited credentials come from, unless `sharedCredentials`. */
  stsEndpoint: string;
  roleArn: string;
  credentialSeconds: number;
}

export interface Edge {
  /** Environments get hostnames below this domain, whose wildcard the operator points at the edge. */
  domain: string;
  /** The port players connect to. */
  port: number;
}

type Env = Record<string, string | undefined>;

export function loadConfig(env: Env = process.env): Config {
  const port = Number(env.PORT ?? "8080");
  if (!Number.isInteger(port) || port < 0 || port > 65_535) {
    throw new Error("PORT must be a port number");
  }
  const secretKey = Buffer.from(required(env, "CHUNK_SECRET_KEY"), "base64");
  if (secretKey.length !== 32) {
    throw new Error("CHUNK_SECRET_KEY must be 32 bytes, base64-encoded (for example `openssl rand -base64 32`)");
  }
  const publicUrl = (env.CHUNK_PUBLIC_URL ?? `http://localhost:${port}`).replace(/\/+$/, "");
  return {
    databaseUrl: required(env, "DATABASE_URL"),
    secretKey,
    operatorToken: token(env, "CHUNK_OPERATOR_TOKEN"),
    publicUrl,
    host: env.HOST ?? "0.0.0.0",
    port,
    dataDir: env.CHUNK_DATA_DIR ?? "data",
    archiveLimits: {
      maxExpandedBytes: positive(env, "CHUNK_MAX_RELEASE_EXPANDED_BYTES", 8 * 1024 ** 3),
      maxEntries: positive(env, "CHUNK_MAX_RELEASE_ENTRIES", 100_000),
    },
    edge: edgeOf(env),
    edgeToken: token(env, "CHUNK_EDGE_TOKEN"),
    logStore: logStoreOf(env),
    machines: machinesOf(env, publicUrl),
    dashboardDir: env.CHUNK_DASHBOARD_DIR || undefined,
  };
}

function machinesOf(env: Env, publicUrl: string): Machines | undefined {
  const image = env.CHUNK_ENVIRONMENT_IMAGE;
  if (!image) return undefined;
  const jvmImage = env.CHUNK_JVM_IMAGE || undefined;
  if (jvmImage !== undefined && !jvmImage.includes("{java}")) {
    throw new Error("CHUNK_JVM_IMAGE must contain {java}, which stands for a release's Java version");
  }
  return {
    dockerHost: env.DOCKER_HOST ?? "unix:///var/run/docker.sock",
    network: env.CHUNK_MACHINE_NETWORK ?? "chunk",
    image,
    jvmImage,
    managementUrl: env.CHUNK_MACHINE_MANAGEMENT_URL ?? publicUrl,
    coreMemoryMib: positive(env, "CHUNK_CORE_MEMORY_MIB", 1024),
    corePort: positive(env, "CHUNK_CORE_PORT", 7070),
    trustedEdges: env.CHUNK_MACHINE_TRUSTED_EDGES || undefined,
    offlineLogins: env.CHUNK_MACHINE_OFFLINE_LOGINS === "1",
  };
}

function token(env: Env, name: string): string | undefined {
  const value = env[name] || undefined;
  if (value !== undefined && value.length < 32) throw new Error(`${name} must be at least 32 characters`);
  return value;
}

function logStoreOf(env: Env): LogStore | undefined {
  const bucket = env.CHUNK_LOG_STORE_BUCKET;
  if (!bucket) return undefined;
  const endpoint = required(env, "CHUNK_LOG_STORE_ENDPOINT");
  const sharedCredentials = env.CHUNK_LOG_STORE_SHARED_CREDENTIALS === "1";
  return {
    endpoint,
    region: env.CHUNK_LOG_STORE_REGION ?? "us-east-1",
    bucket,
    prefix: env.CHUNK_LOG_STORE_PREFIX ?? "environments/",
    accessKeyId: required(env, "CHUNK_LOG_STORE_ACCESS_KEY_ID"),
    secretAccessKey: required(env, "CHUNK_LOG_STORE_SECRET_ACCESS_KEY"),
    sharedCredentials,
    stsEndpoint: env.CHUNK_LOG_STORE_STS_ENDPOINT ?? endpoint,
    roleArn: sharedCredentials ? "" : required(env, "CHUNK_LOG_STORE_ROLE_ARN"),
    credentialSeconds: positive(env, "CHUNK_LOG_STORE_CREDENTIAL_SECONDS", 3600),
  };
}

function edgeOf(env: Env): Edge | undefined {
  const domain = env.CHUNK_EDGE_DOMAIN?.toLowerCase().replace(/\.$/, "");
  if (!domain) return undefined;
  const port = Number(env.CHUNK_EDGE_PORT ?? "25565");
  if (!Number.isInteger(port) || port < 1 || port > 65_535) throw new Error("CHUNK_EDGE_PORT must be a port number");
  return { domain, port };
}

function positive(env: Env, name: string, fallback: number): number {
  const value = Number(env[name] ?? fallback);
  if (!Number.isSafeInteger(value) || value < 1) throw new Error(`${name} must be a positive integer`);
  return value;
}

function required(env: Env, name: string): string {
  const value = env[name];
  if (!value) throw new Error(`${name} is required`);
  return value;
}
