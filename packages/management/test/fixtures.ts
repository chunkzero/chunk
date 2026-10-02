import { createHash, randomBytes } from "node:crypto";

import { encodeRevision } from "../src/assets/revision.ts";

const encoder = new TextEncoder();
const sha256 = (bytes: Uint8Array) => createHash("sha256").update(bytes).digest("hex");

/** A GNU tar header as tar-rs writes it, with a valid checksum unless `badChecksum`. */
function header(name: string, size: number, type: string, badChecksum = false): Uint8Array {
  const block = new Uint8Array(512);
  const octal = (value: number, length: number) => encoder.encode(`${value.toString(8).padStart(length - 1, "0")}\0`);
  block.set(encoder.encode(name).subarray(0, 100), 0);
  block.set(octal(0o644, 8), 100);
  block.set(octal(0, 8), 108);
  block.set(octal(0, 8), 116);
  block.set(octal(size, 12), 124);
  block.set(octal(0, 12), 136);
  block[156] = type.charCodeAt(0);
  block.set(encoder.encode("ustar  \0"), 257);
  block.fill(0x20, 148, 156);
  const sum = block.reduce((total, byte) => total + byte, 0) + (badChecksum ? 1 : 0);
  block.set(encoder.encode(`${sum.toString(8).padStart(6, "0")}\0 `), 148);
  return block;
}

function padded(data: Uint8Array): Uint8Array {
  const out = new Uint8Array(Math.ceil(data.byteLength / 512) * 512);
  out.set(data);
  return out;
}

function tar(files: [string, Uint8Array][], badChecksum = false) {
  const blocks: Uint8Array[] = [];
  files.forEach(([name, data], index) => {
    if (encoder.encode(name).byteLength > 100) {
      const longName = encoder.encode(`${name}\0`);
      blocks.push(header("././@LongLink", longName.byteLength, "L"), padded(longName));
    }
    blocks.push(header(name, data.byteLength, "0", badChecksum && index === 0), padded(data));
  });
  blocks.push(new Uint8Array(1024));
  return Buffer.concat(blocks);
}

export interface ArchiveOptions {
  /** Rewrites release.json before it is written. */
  manifest?: (manifest: Record<string, unknown>) => unknown;
  /** Leaves out the paths it accepts. */
  omit?: (path: string) => boolean;
  /** Replaces files' contents by path. */
  replace?: Record<string, string>;
  /** Adds files after the release's own. */
  extra?: [string, string][];
  badChecksum?: boolean;
}

/**
 * A gzip-compressed tar laid out like `chunk build`'s: app JARs named by digest, a file whose path needs a GNU long
 * name, backend files and release.json, whose apps declare no worlds or packs unless `options.manifest` adds them.
 */
export function releaseArchive(
  id: string,
  apps: { id: string; sessions: string[] }[] = [{ id: "lobby", sessions: ["default"] }],
  options: ArchiveOptions = {},
) {
  const files: [string, Uint8Array][] = [];
  const artifacts = apps.map((app, index) => {
    const jar = randomBytes(index === 0 ? 200_000 : 1000);
    const digest = sha256(jar);
    files.push([`apps/${app.id}/${digest}.jar`, jar]);
    return {
      id: app.id,
      jar: `apps/${app.id}/${digest}.jar`,
      sha256: digest,
      java_version: 25,
      sessions: Object.fromEntries(
        app.sessions.map((session) => [session, { machine_profile: "default", capacity: 16 }]),
      ),
    };
  });
  files.push([`backend/${"a".repeat(120)}.txt`, encoder.encode("hello")]);
  const source = "export default {};\n";
  const contract = { contract_version: 2, runtime_profile: "transactional_v1", tables: {}, functions: {} };
  files.push(
    ["source.mjs", encoder.encode(source)],
    ["contract.json", encoder.encode(JSON.stringify(contract))],
    ["backend.json", encoder.encode(JSON.stringify({ ...contract, id, source }))],
  );
  const manifest = {
    id,
    version: 4,
    java_version: 25,
    apps: artifacts,
    profiles: { default: { memory_mib: 1024, max_sessions: 16 } },
    assets: {},
  };
  files.push(["release.json", encoder.encode(JSON.stringify(options.manifest?.(manifest) ?? manifest))]);
  const kept = files
    .filter(([name]) => !options.omit?.(name))
    .map(([name, bytes]): [string, Uint8Array] => {
      const replacement = options.replace && Object.hasOwn(options.replace, name) ? options.replace[name] : undefined;
      return [name, replacement === undefined ? bytes : encoder.encode(replacement)];
    })
    .concat((options.extra ?? []).map(([name, text]): [string, Uint8Array] => [name, encoder.encode(text)]));
  const bytes = Bun.gzipSync(tar(kept, options.badChecksum));
  return { bytes, sha256: sha256(bytes), sizeBytes: BigInt(bytes.byteLength) };
}

/** A gzip-compressed tar holding just these files. */
export function rawArchive(files: [string, string][]) {
  const bytes = Bun.gzipSync(tar(files.map(([name, text]) => [name, encoder.encode(text)])));
  return { bytes, sha256: sha256(bytes), sizeBytes: BigInt(bytes.byteLength) };
}

/** An asset revision's canonical manifest, its ID and its blobs' bytes by SHA-256, from the bytes of each entry. */
export function assetRevision({
  packs = {},
  shared = {},
  apps = {},
}: {
  packs?: Record<string, string>;
  shared?: Record<string, string>;
  apps?: Record<string, { worlds?: Record<string, string>; files?: Record<string, string> }>;
} = {}) {
  const blobs = new Map<string, Uint8Array>();
  const blob = (text: string) => {
    const bytes = encoder.encode(text);
    blobs.set(sha256(bytes), bytes);
    return { sha256: sha256(bytes), size: bytes.byteLength };
  };
  const map = <T>(entries: Record<string, string>, read: (text: string) => T) =>
    new Map(Object.entries(entries).map(([key, text]) => [key, read(text)]));
  const manifest = encodeRevision({
    version: 1,
    packs: map(packs, (text) => ({
      ...blob(text),
      sha1: createHash("sha1").update(encoder.encode(text)).digest("hex"),
    })),
    shared: map(shared, blob),
    apps: new Map(
      Object.entries(apps).map(([app, assets]) => [
        app,
        { worlds: map(assets.worlds ?? {}, blob), files: map(assets.files ?? {}, blob) },
      ]),
    ),
  });
  return { manifest, id: sha256(manifest), blobs };
}
