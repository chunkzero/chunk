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
  const operatorToken = env.CHUNK_OPERATOR_TOKEN || undefined;
  if (operatorToken !== undefined && operatorToken.length < 32) {
    throw new Error("CHUNK_OPERATOR_TOKEN must be at least 32 characters");
  }
  return {
    databaseUrl: required(env, "DATABASE_URL"),
    secretKey,
    operatorToken,
    publicUrl: (env.CHUNK_PUBLIC_URL ?? `http://localhost:${port}`).replace(/\/+$/, ""),
    host: env.HOST ?? "0.0.0.0",
    port,
    dataDir: env.CHUNK_DATA_DIR ?? "data",
    archiveLimits: {
      maxExpandedBytes: positive(env, "CHUNK_MAX_RELEASE_EXPANDED_BYTES", 8 * 1024 ** 3),
      maxEntries: positive(env, "CHUNK_MAX_RELEASE_ENTRIES", 100_000),
    },
    edge: edgeOf(env),
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
