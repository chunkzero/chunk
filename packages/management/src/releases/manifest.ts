import { createHash } from "node:crypto";

import { type ArchiveLimits, scanArchive } from "./archive.ts";

/**
 * The parts of `release.json` (crates/chunk-build/src/release.rs) management reads. The environment validates the
 * rest of a release when it loads it.
 */
export interface ReleaseManifest {
  id: string;
  /** The Java version every app in the release runs on; undefined when release.json declares none management can use. */
  java_version: number | undefined;
  apps: { id: string; sessions: Record<string, { machine_profile: string; capacity: number }> }[];
  profiles: Record<string, { memory_mib: number; max_sessions: number }>;
}

const manifestPath = "release.json";
const maxManifestBytes = 16 * 1024 * 1024;
const manifestVersion = 3;

/**
 * Checks a stored archive in one pass: its size and digest against the declaration, its tar structure within
 * `limits`, and the parts of `release.json` management reads. Returns those parts; throws with a reason.
 */
export async function verifyRelease(
  archive: ReadableStream<Uint8Array>,
  expected: { releaseId: string; sha256: string; sizeBytes: bigint },
  limits: ArchiveLimits,
): Promise<ReleaseManifest> {
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
  const entries = await scanArchive(counted, limits, keepLimit);
  if (sizeBytes !== expected.sizeBytes || hash.digest("hex") !== expected.sha256) {
    throw new Error("the archive does not match the declared size and digest");
  }

  const bytes = entries.get(manifestPath)?.data;
  if (!bytes) throw new Error("the archive has no release.json");
  const manifest = readManifest(bytes);
  if (typeof manifest === "string") throw new Error(`release.json ${manifest}`);
  if (manifest.id !== expected.releaseId) {
    throw new Error(`release.json names release ${manifest.id}, not ${expected.releaseId}`);
  }
  return manifest;
}

/** How many bytes of an entry verification keeps in memory: only release.json, capped. */
export function keepLimit(path: string): number | undefined {
  return path === manifestPath ? maxManifestBytes : undefined;
}

/** The fields management reads, or what is wrong with them. Ignores every other field. */
function readManifest(bytes: Uint8Array): ReleaseManifest | string {
  let value: unknown;
  try {
    value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes));
  } catch {
    return "is not UTF-8 JSON";
  }
  if (!isRecord(value)) return "is not a JSON object";
  if (value.version !== manifestVersion) return `is not version ${manifestVersion}`;
  if (typeof value.id !== "string") return "has no id";
  if (!isRecord(value.profiles)) return "has no profiles";
  const profiles: [string, ReleaseManifest["profiles"][string]][] = [];
  for (const [name, profile] of Object.entries(value.profiles)) {
    if (!isName(name, profilePattern) || !isRecord(profile)) return "has an invalid profile";
    const { memory_mib, max_sessions } = profile;
    if (!isCount(memory_mib) || !isCount(max_sessions)) return `has an invalid profile ${name}`;
    profiles.push([name, { memory_mib, max_sessions }]);
  }
  if (!Array.isArray(value.apps)) return "has no apps";
  const apps: ReleaseManifest["apps"] = [];
  const appIds = new Set<string>();
  for (const app of value.apps as unknown[]) {
    if (!isRecord(app) || !isName(app.id, namePattern)) return "has an invalid app";
    const appId = app.id;
    if (appIds.has(appId)) return `lists app ${appId} twice`;
    appIds.add(appId);
    if (!isRecord(app.sessions)) return `has no sessions for ${appId}`;
    const sessions: [string, ReleaseManifest["apps"][number]["sessions"][string]][] = [];
    for (const [name, session] of Object.entries(app.sessions)) {
      if (!isName(name, namePattern) || !isRecord(session)) return `has an invalid session for ${appId}`;
      const { machine_profile, capacity } = session;
      if (!isName(machine_profile, profilePattern) || !isCount(capacity)) {
        return `has an invalid session ${name} for ${appId}`;
      }
      sessions.push([name, { machine_profile, capacity }]);
    }
    // fromEntries defines own properties, so a name like `__proto__` stays a plain key.
    apps.push({ id: appId, sessions: Object.fromEntries(sessions) });
  }
  const java_version = isJavaVersion(value.java_version) ? value.java_version : undefined;
  return { id: value.id, java_version, apps, profiles: Object.fromEntries(profiles) };
}

/** chunk_contract's app and session names. */
const namePattern = /^[A-Za-z_][A-Za-z0-9_]{0,127}$/;
const profilePattern = /^[A-Za-z0-9_-]{1,128}$/;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isName(value: unknown, pattern: RegExp): value is string {
  return typeof value === "string" && pattern.test(value);
}

/** Well past any real Java release, and within capacity requests' `integer` column. */
function isJavaVersion(value: unknown): value is number {
  return typeof value === "number" && Number.isInteger(value) && value >= 1 && value <= 1000;
}

function isCount(value: unknown): value is number {
  return typeof value === "number" && Number.isInteger(value) && value >= 1 && value <= 0xffff_ffff;
}
