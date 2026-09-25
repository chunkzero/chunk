import { createHash, randomBytes } from "node:crypto";

const encoder = new TextEncoder();

function tarEntry(name: string, data: Uint8Array, type = "0"): Uint8Array {
  const header = new Uint8Array(512);
  const octal = (value: number, length: number) => encoder.encode(`${value.toString(8).padStart(length - 1, "0")}\0`);
  header.set(encoder.encode(name).subarray(0, 100), 0);
  header.set(octal(0o644, 8), 100);
  header.set(octal(data.byteLength, 12), 124);
  header[156] = type.charCodeAt(0);
  header.set(encoder.encode("ustar\u000000"), 257);
  const body = new Uint8Array(Math.ceil(data.byteLength / 512) * 512);
  body.set(data);
  return Buffer.concat([header, body]);
}

/**
 * A gzip-compressed tar like `chunk build` writes: a large JAR with a GNU long name ahead of `release.json`, then
 * the end-of-archive blocks.
 */
export function releaseArchive(
  manifest: { id: string; apps: { id: string; sessions: string[] }[] },
  { includeManifest = true } = {},
) {
  const release = {
    id: manifest.id,
    version: 3,
    java_version: 21,
    apps: manifest.apps.map((app) => ({
      id: app.id,
      jar: `apps/${app.id}/app.jar`,
      sha256: "0".repeat(64),
      java_version: 21,
      sessions: Object.fromEntries(app.sessions.map((id) => [id, { machine_profile: "default", capacity: 16 }])),
    })),
    profiles: {},
    assets: {},
  };
  const longName = `apps/${"a".repeat(150)}/app.jar`;
  const tar = Buffer.concat([
    tarEntry("././@LongLink", encoder.encode(longName), "L"),
    tarEntry("ignored", randomBytes(200_000)),
    ...(includeManifest ? [tarEntry("release.json", encoder.encode(JSON.stringify(release)))] : []),
    new Uint8Array(1024),
  ]);
  const bytes = Bun.gzipSync(tar);
  return { bytes, sha256: createHash("sha256").update(bytes).digest("hex"), sizeBytes: BigInt(bytes.byteLength) };
}
