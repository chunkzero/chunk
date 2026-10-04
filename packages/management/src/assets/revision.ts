// chunk_contract's asset revisions and contracts (crates/chunk-contract/src/assets.rs).
import { createHash } from "node:crypto";

export interface AssetBlob {
  sha256: string;
  size: number;
}

export interface PackBlob extends AssetBlob {
  sha1: string;
}

export interface AssetRevision {
  version: number;
  packs: Map<string, PackBlob>;
  shared: Map<string, AssetBlob>;
  apps: Map<string, { worlds: Map<string, AssetBlob>; files: Map<string, AssetBlob> }>;
}

/** What a release declares its apps need from the revision deployed with it, as release.json holds it. */
export interface AssetContract {
  worlds: Record<string, string[]>;
  packs: Record<string, { required: boolean; prompt: string | undefined }>;
  app_packs: Record<string, string[]>;
}

export const maxManifestBytes = 4 * 1024 * 1024;
const revisionVersion = 1;
const maxEntries = 4096;
const maxWorldBytes = 256 * 1024 * 1024;
const maxPackBytes = 250 * 1024 * 1024;
const maxFileBytes = 128 * 1024 * 1024;
const maxRevisionBytes = 4 * 1024 * 1024 * 1024;
const maxPathBytes = 512;
const maxPromptBytes = 1024;

const namePattern = /^[A-Za-z_][A-Za-z0-9_]{0,127}$/;
const segmentPattern = /^[A-Za-z0-9._-]+$/;

export function sha256Hex(bytes: Uint8Array): string {
  return createHash("sha256").update(bytes).digest("hex");
}

/**
 * Parses and validates a revision from exactly its canonical JSON, whose SHA-256 is its ID: the JSON `encode` writes
 * back must be the same bytes. Throws with a reason.
 */
export function decodeRevision(bytes: Uint8Array): AssetRevision {
  if (bytes.byteLength > maxManifestBytes) throw new Error(`an asset revision is at most ${maxManifestBytes} bytes`);
  let value: unknown;
  try {
    value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes));
  } catch {
    throw new Error("invalid asset revision: not UTF-8 JSON");
  }
  const root = fields(value, ["version", "packs", "shared", "apps"], "asset revision");
  if (root.version !== revisionVersion) throw new Error("unsupported asset revision version");
  const packs = entries(root.packs, "packs", (pack, name) => {
    const blob = fields(pack, ["sha256", "sha1", "size"], `pack ${name}`);
    if (
      !namePattern.test(name) ||
      !digest(blob.sha256, 64) ||
      !digest(blob.sha1, 40) ||
      !size(blob.size, maxPackBytes)
    ) {
      throw new Error(`invalid pack ${name}`);
    }
    return { sha256: blob.sha256, sha1: blob.sha1, size: blob.size };
  });
  const shared = entries(root.shared, "shared", file);
  const apps = entries(root.apps, "apps", (assets, app) => {
    if (!namePattern.test(app)) throw new Error(`invalid app ID ${app}`);
    const { worlds, files } = fields(assets, ["worlds", "files"], `assets of app ${app}`);
    return {
      worlds: entries(worlds, "worlds", (world, name) => {
        const blob = fields(world, ["sha256", "size"], `world ${app}/${name}`);
        if (!namePattern.test(name) || !digest(blob.sha256, 64) || !size(blob.size, maxWorldBytes)) {
          throw new Error(`invalid world ${app}/${name}`);
        }
        return { sha256: blob.sha256, size: blob.size };
      }),
      files: entries(files, "files", file),
    };
  });
  const revision: AssetRevision = { version: revisionVersion, packs, shared, apps };
  let count = packs.size + shared.size;
  for (const assets of apps.values()) count += assets.worlds.size + assets.files.size;
  if (count > maxEntries) throw new Error(`an asset revision holds at most ${maxEntries} entries`);
  if ([...revisionBlobs(revision).values()].reduce((total, blob) => total + blob.size, 0) > maxRevisionBytes) {
    throw new Error("asset revision size limit");
  }
  if (!Buffer.from(encodeRevision(revision)).equals(bytes)) throw new Error("asset revision JSON is not canonical");
  return revision;
}

/** The canonical JSON: fields in declaration order, map keys in byte order, no whitespace. */
export function encodeRevision(revision: AssetRevision): Uint8Array {
  const map = <T>(entries: Map<string, T>, value: (item: T) => string) =>
    `{${[...entries]
      .sort(([a], [b]) => (a < b ? -1 : 1))
      .map(([key, item]) => `${JSON.stringify(key)}:${value(item)}`)
      .join(",")}}`;
  const blob = ({ sha256, size }: AssetBlob) => `{"sha256":${JSON.stringify(sha256)},"size":${size}}`;
  const pack = ({ sha256, sha1, size }: PackBlob) =>
    `{"sha256":${JSON.stringify(sha256)},"sha1":${JSON.stringify(sha1)},"size":${size}}`;
  const apps = map(
    revision.apps,
    (assets) => `{"worlds":${map(assets.worlds, blob)},"files":${map(assets.files, blob)}}`,
  );
  return new TextEncoder().encode(
    `{"version":${revision.version},"packs":${map(revision.packs, pack)},"shared":${map(revision.shared, blob)},"apps":${apps}}`,
  );
}

/** Every distinct blob of the revision, by SHA-256, and whether it is a pack's. Throws on one digest with two sizes. */
export function revisionBlobs(revision: AssetRevision): Map<string, { size: number; pack: boolean }> {
  const blobs = new Map<string, { size: number; pack: boolean }>();
  const add = ({ sha256, size }: AssetBlob, pack: boolean) => {
    const known = blobs.get(sha256);
    if (known !== undefined && known.size !== size) throw new Error(`blob ${sha256} is declared with different sizes`);
    blobs.set(sha256, { size, pack: pack || (known?.pack ?? false) });
  };
  for (const blob of revision.packs.values()) add(blob, true);
  for (const blob of revision.shared.values()) add(blob, false);
  for (const assets of revision.apps.values()) {
    for (const blob of [...assets.worlds.values(), ...assets.files.values()]) add(blob, false);
  }
  return blobs;
}

/** Reads a release's `assets`, or throws with what is wrong with it. */
export function readContract(value: unknown): AssetContract {
  const contract = optionalFields(value, ["worlds", "packs", "app_packs"], "assets");
  const worlds = record(contract.worlds, "assets.worlds", (names, app) => {
    if (!namePattern.test(app) || !Array.isArray(names) || !names.every((name) => isName(name))) {
      throw new Error(`invalid world declarations of app ${app}`);
    }
    return names as string[];
  });
  const packs = record(contract.packs, "assets.packs", (declaration, name) => {
    const { required = false, prompt } = optionalFields(declaration, ["required", "prompt"], `pack ${name}`);
    const promptValid =
      prompt === undefined || (typeof prompt === "string" && Buffer.byteLength(prompt) <= maxPromptBytes);
    if (!namePattern.test(name) || typeof required !== "boolean" || !promptValid) {
      throw new Error(`invalid pack declaration ${name}`);
    }
    return { required, prompt };
  });
  const appPacks = record(contract.app_packs, "assets.app_packs", (names, app) => {
    const valid =
      namePattern.test(app) &&
      Array.isArray(names) &&
      new Set(names).size === names.length &&
      names.every((name) => typeof name === "string" && Object.hasOwn(packs, name));
    if (!valid) throw new Error(`invalid packs of app ${app}`);
    return names as string[];
  });
  return { worlds, packs, app_packs: appPacks };
}

/** Throws naming the first declared world or pack `revision` lacks. */
export function checkContract(contract: AssetContract, revision: AssetRevision): void {
  for (const [app, worlds] of Object.entries(contract.worlds)) {
    for (const world of worlds) {
      if (!revision.apps.get(app)?.worlds.has(world)) {
        throw new Error(`the asset revision has no world ${world} for app ${app}`);
      }
    }
  }
  const pack = Object.keys(contract.packs).find((name) => !revision.packs.has(name));
  if (pack !== undefined) throw new Error(`the asset revision has no pack ${pack}`);
}

function file(value: unknown, path: string): AssetBlob {
  const blob = fields(value, ["sha256", "size"], `asset file ${path}`);
  const pathValid =
    Buffer.byteLength(path) <= maxPathBytes &&
    path.split("/").every((segment) => segmentPattern.test(segment) && segment !== "." && segment !== "..");
  if (!pathValid || !digest(blob.sha256, 64) || !size(blob.size, maxFileBytes)) {
    throw new Error(`invalid asset file ${path}`);
  }
  return { sha256: blob.sha256, size: blob.size };
}

/** An object with exactly `keys`, as serde's `deny_unknown_fields` structs without defaults read. */
function fields<K extends string>(value: unknown, keys: readonly K[], what: string): Record<K, unknown> {
  const object = optionalFields(value, keys, what);
  if (keys.some((key) => !Object.hasOwn(object, key))) throw new Error(`invalid asset revision: ${what} lacks a field`);
  return object as Record<K, unknown>;
}

/** An object with no keys but `keys`. */
function optionalFields<K extends string>(
  value: unknown,
  keys: readonly K[],
  what: string,
): Partial<Record<K, unknown>> {
  const unknown = Object.keys(object(value, what)).find((key) => !(keys as readonly string[]).includes(key));
  if (unknown !== undefined) throw new Error(`${what} has an unknown field ${unknown}`);
  return value as Partial<Record<K, unknown>>;
}

function object(value: unknown, what: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error(`${what} is not an object`);
  return value as Record<string, unknown>;
}

function entries<T>(value: unknown, what: string, read: (item: unknown, key: string) => T): Map<string, T> {
  return new Map(Object.entries(object(value, what)).map(([key, item]) => [key, read(item, key)]));
}

function record<T>(value: unknown, what: string, read: (item: unknown, key: string) => T): Record<string, T> {
  if (value === undefined) return {};
  // fromEntries defines own properties, so a name like `__proto__` stays a plain key.
  return Object.fromEntries(entries(value, what, read));
}

function isName(value: unknown): value is string {
  return typeof value === "string" && namePattern.test(value);
}

function digest(value: unknown, length: number): value is string {
  return typeof value === "string" && value.length === length && /^[0-9a-f]*$/.test(value);
}

function size(value: unknown, max: number): value is number {
  return typeof value === "number" && Number.isInteger(value) && value >= 0 && value <= max;
}
