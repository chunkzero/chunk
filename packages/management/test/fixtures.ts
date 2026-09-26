import { createHash, randomBytes } from "node:crypto";

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
  badChecksum?: boolean;
}

/**
 * A gzip-compressed tar laid out like `chunk build`'s: app JARs named by digest, an asset whose path needs a GNU
 * long name, backend files and release.json.
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
      java_version: 21,
      sessions: Object.fromEntries(
        app.sessions.map((session) => [session, { machine_profile: "default", capacity: 16 }]),
      ),
    };
  });
  const asset = encoder.encode("hello");
  const assetPath = `assets/${"a".repeat(120)}.txt`;
  files.push([assetPath, asset]);
  for (const name of ["backend.json", "contract.json", "source.mjs"]) files.push([name, encoder.encode("{}")]);
  const manifest = {
    id,
    version: 3,
    java_version: 21,
    apps: artifacts,
    profiles: { default: { memory_mib: 1024, max_sessions: 16 } },
    assets: { [assetPath]: sha256(asset) },
  };
  files.push(["release.json", encoder.encode(JSON.stringify(options.manifest?.(manifest) ?? manifest))]);
  const kept = files.filter(([name]) => !options.omit?.(name));
  const bytes = Bun.gzipSync(tar(kept, options.badChecksum));
  return { bytes, sha256: sha256(bytes), sizeBytes: BigInt(bytes.byteLength) };
}

/** A gzip-compressed tar holding just these files. */
export function rawArchive(files: [string, string][]) {
  const bytes = Bun.gzipSync(tar(files.map(([name, text]) => [name, encoder.encode(text)])));
  return { bytes, sha256: sha256(bytes), sizeBytes: BigInt(bytes.byteLength) };
}
