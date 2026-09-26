import { createHash } from "node:crypto";

import { type ArchiveLimits, scanArchive } from "./archive.ts";

/** `release.json` as `chunk build` writes it (crates/chunk-build/src/release.rs, chunk_contract::AppArtifact). */
export interface ReleaseManifest {
  id: string;
  version: number;
  java_version: number;
  apps: {
    id: string;
    jar: string;
    sha256: string;
    java_version: number;
    sessions: Record<string, { machine_profile: string; capacity: number }>;
  }[];
  profiles: Record<string, { memory_mib: number; max_sessions: number }>;
  assets: Record<string, string>;
}

const manifestPath = "release.json";
const maxManifestBytes = 16 * 1024 * 1024;
const manifestVersion = 3;
/** The backend files every release carries besides its apps and assets, with chunk_contract's size limits. */
const backendFiles: Record<string, number> = {
  "backend.json": 5 * 1024 * 1024,
  "contract.json": 2 * 1024 * 1024,
  "source.mjs": 4 * 1024 * 1024,
};
/** chunk_contract::CONTRACT_VERSION and the runtime profiles it knows. */
const contractVersion = 2;
const runtimeProfiles = ["transactional_v1"];
/** Fields of `contract.json`; `backend.json` adds `id` and `source`. */
const contractFields = ["contract_version", "runtime_profile", "tables", "functions"];
const optionalContractFields = ["domains", "session_methods", "session_configurations", "destinations"];

/**
 * Checks a stored archive in one pass: its size and digest against the declaration, its tar structure within
 * `limits`, `release.json`, and every payload the manifest names. Returns the manifest text; throws with a reason.
 */
export async function verifyRelease(
  archive: ReadableStream<Uint8Array>,
  expected: { releaseId: string; sha256: string; sizeBytes: bigint },
  limits: ArchiveLimits,
): Promise<string> {
  const hash = createHash("sha256");
  let sizeBytes = 0n;
  const counted = archive.pipeThrough(
    new TransformStream<Uint8Array, Uint8Array>({
      transform(chunk, controller) {
        sizeBytes += BigInt(chunk.byteLength);
        if (sizeBytes > expected.sizeBytes) throw new Error("the archive is larger than declared");
        hash.update(chunk);
        controller.enqueue(chunk);
      },
    }),
  );
  const entries = await scanArchive(counted, limits, (path) =>
    path === manifestPath ? maxManifestBytes : backendFiles[path],
  );
  if (sizeBytes !== expected.sizeBytes || hash.digest("hex") !== expected.sha256) {
    throw new Error("the archive does not match the declared size and digest");
  }

  const bytes = entries.get(manifestPath)?.data;
  if (!bytes) throw new Error("the archive has no release.json");
  const text = new TextDecoder().decode(bytes);
  const manifest: unknown = JSON.parse(text);
  const problem = manifestProblem(manifest);
  if (problem !== undefined) throw new Error(`release.json ${problem}`);
  const { id, apps, assets } = manifest as ReleaseManifest;
  if (id !== expected.releaseId) throw new Error(`release.json names release ${id}, not ${expected.releaseId}`);

  const payloads: [string, string | undefined][] = [
    ...apps.map((app): [string, string] => [app.jar, app.sha256]),
    ...Object.entries(assets),
    ...Object.keys(backendFiles).map((path): [string, undefined] => [path, undefined]),
  ];
  for (const [path, sha256] of payloads) {
    const entry = entries.get(path);
    if (!entry) throw new Error(`the archive is missing ${path}`);
    if (sha256 !== undefined && entry.sha256 !== sha256) throw new Error(`${path} does not match its digest`);
  }
  const backend = backendProblem(id, (path) => entries.get(path)?.data ?? new Uint8Array());
  if (backend !== undefined) throw new Error(backend);
  return text;
}

/**
 * Checks the backend files the way `chunk build` writes them: `contract.json` is the compiled backend metadata, and
 * `backend.json` is that metadata plus the release ID and the exact text of `source.mjs`.
 */
function backendProblem(releaseId: string, read: (path: string) => Uint8Array): string | undefined {
  let source: string;
  try {
    source = new TextDecoder("utf-8", { fatal: true }).decode(read("source.mjs"));
  } catch {
    return "source.mjs is not UTF-8";
  }
  const contract = parseJson(read("contract.json"));
  if (!isRecord(contract)) return "contract.json is not a JSON object";
  const problem = contractProblem(contract);
  if (problem !== undefined) return `contract.json ${problem}`;
  const backend = parseJson(read("backend.json"));
  if (!isRecord(backend)) return "backend.json is not a JSON object";
  const { id, source: backendSource, ...metadata } = backend;
  if (id !== releaseId) return `backend.json names release ${JSON.stringify(id)}, not ${releaseId}`;
  if (backendSource !== source) return "backend.json does not carry source.mjs";
  if (!sameJson(metadata, contract)) return "backend.json does not match contract.json";
  return undefined;
}

function contractProblem(contract: Record<string, unknown>): string | undefined {
  const known = [...contractFields, ...optionalContractFields];
  const unknown = Object.keys(contract).find((key) => !known.includes(key));
  if (unknown !== undefined) return `has an unexpected field ${unknown}`;
  if (contract.contract_version !== contractVersion) return `is not contract version ${contractVersion}`;
  if (!runtimeProfiles.includes(contract.runtime_profile as string)) return "has an unknown runtime_profile";
  if (!isRecord(contract.tables)) return "has no tables";
  if (!isRecord(contract.functions) || Object.keys(contract.functions).length > 256) return "has invalid functions";
  for (const field of optionalContractFields) {
    if (field in contract && !isRecord(contract[field])) return `has an invalid ${field}`;
  }
  return undefined;
}

function parseJson(bytes: Uint8Array): unknown {
  try {
    return JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes));
  } catch {
    return undefined;
  }
}

/** Deep equality of parsed JSON, ignoring object key order. */
function sameJson(a: unknown, b: unknown): boolean {
  if (Array.isArray(a) || Array.isArray(b)) {
    return Array.isArray(a) && Array.isArray(b) && a.length === b.length && a.every((item, i) => sameJson(item, b[i]));
  }
  if (isRecord(a) && isRecord(b)) {
    const keys = Object.keys(a);
    return (
      keys.length === Object.keys(b).length && keys.every((key) => Object.hasOwn(b, key) && sameJson(a[key], b[key]))
    );
  }
  return a === b;
}

/** Describes the first way `value` differs from a release manifest, or undefined when it is one. */
function manifestProblem(value: unknown): string | undefined {
  if (!isRecord(value) || !hasOnly(value, ["id", "version", "java_version", "apps", "profiles", "assets"])) {
    return "has unexpected fields";
  }
  if (typeof value.id !== "string") return "has no id";
  if (value.version !== manifestVersion) return `is not version ${manifestVersion}`;
  if (!isCount(value.java_version)) return "has no java_version";
  if (!isRecord(value.profiles)) return "has no profiles";
  for (const [name, profile] of Object.entries(value.profiles)) {
    if (
      !isRecord(profile) ||
      !hasOnly(profile, ["memory_mib", "max_sessions"]) ||
      !isCount(profile.memory_mib) ||
      !isCount(profile.max_sessions)
    ) {
      return `has an invalid profile ${name}`;
    }
  }
  if (!isRecord(value.assets) || !Object.values(value.assets).every(isDigest)) return "has invalid assets";
  if (!Array.isArray(value.apps)) return "has no apps";
  const ids = new Set<string>();
  for (const app of value.apps as unknown[]) {
    if (!isRecord(app) || !hasOnly(app, ["id", "jar", "sha256", "java_version", "sessions"]) || !isName(app.id)) {
      return "has an invalid app";
    }
    if (ids.has(app.id)) return `lists app ${app.id} twice`;
    ids.add(app.id);
    if (!isDigest(app.sha256) || app.jar !== `apps/${app.id}/${app.sha256}.jar`)
      return `has an invalid jar for ${app.id}`;
    if (!isCount(app.java_version)) return `has no java_version for ${app.id}`;
    if (!isRecord(app.sessions)) return `has no sessions for ${app.id}`;
    const sessions = Object.entries(app.sessions);
    if (sessions.length === 0 || sessions.length > 128) return `must give ${app.id} 1 to 128 sessions`;
    for (const [session, declaration] of sessions) {
      if (
        !isName(session) ||
        !isRecord(declaration) ||
        !hasOnly(declaration, ["machine_profile", "capacity"]) ||
        typeof declaration.machine_profile !== "string" ||
        !/^[A-Za-z0-9_-]{1,128}$/.test(declaration.machine_profile) ||
        !isCount(declaration.capacity) ||
        declaration.capacity > 128
      ) {
        return `has an invalid session ${session} for ${app.id}`;
      }
    }
  }
  return undefined;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function hasOnly(value: Record<string, unknown>, keys: string[]): boolean {
  return Object.keys(value).length === keys.length && keys.every((key) => Object.hasOwn(value, key));
}

/** chunk_contract's app and session names. */
function isName(value: unknown): value is string {
  return typeof value === "string" && /^[A-Za-z_][A-Za-z0-9_]{0,127}$/.test(value);
}

function isDigest(value: unknown): value is string {
  return typeof value === "string" && /^[0-9a-f]{64}$/.test(value);
}

function isCount(value: unknown): value is number {
  return typeof value === "number" && Number.isInteger(value) && value >= 1 && value <= 0xffff_ffff;
}
