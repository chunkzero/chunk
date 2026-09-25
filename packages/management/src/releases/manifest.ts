import { createHash } from "node:crypto";

import { readTarFile } from "./archive.ts";

/** The parts of `release.json` (written by `chunk build`) that this service reads. */
export interface ReleaseManifest {
  id: string;
  apps: { id: string; sessions: Record<string, unknown> }[];
}

const maxManifestBytes = 16 * 1024 * 1024;

export async function digest(archive: ReadableStream<Uint8Array>): Promise<{ sha256: string; sizeBytes: bigint }> {
  const hash = createHash("sha256");
  let sizeBytes = 0n;
  for await (const chunk of archive) {
    hash.update(chunk);
    sizeBytes += BigInt(chunk.byteLength);
  }
  return { sha256: hash.digest("hex"), sizeBytes };
}

/** Reads and checks the archive's `release.json`, returning its text. Throws with a reason when it is unusable. */
export async function readManifest(archive: ReadableStream<Uint8Array>, releaseId: string): Promise<string> {
  const bytes = await readTarFile(archive, "release.json", maxManifestBytes);
  if (!bytes) throw new Error("the archive has no release.json");
  const text = new TextDecoder().decode(bytes);
  const manifest: unknown = JSON.parse(text);
  if (!isManifest(manifest)) throw new Error("release.json does not list apps and their sessions");
  if (manifest.id !== releaseId) throw new Error(`release.json names release ${manifest.id}, not ${releaseId}`);
  return text;
}

function isManifest(value: unknown): value is ReleaseManifest {
  return (
    isRecord(value) &&
    typeof value.id === "string" &&
    Array.isArray(value.apps) &&
    value.apps.every((app: unknown) => isRecord(app) && typeof app.id === "string" && isRecord(app.sessions))
  );
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
