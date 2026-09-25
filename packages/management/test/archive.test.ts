import { expect, test } from "bun:test";

import { readTarFile } from "../src/releases/archive.ts";
import { readManifest } from "../src/releases/manifest.ts";
import { releaseArchive } from "./fixtures.ts";

const streamOf = (bytes: Uint8Array) => new Blob([bytes]).stream();

test("finds release.json behind long names and large entries", async () => {
  const archive = releaseArchive({ id: "r1", apps: [{ id: "lobby", sessions: ["default", "duel"] }] });
  const text = await readManifest(streamOf(archive.bytes), "r1");
  expect(JSON.parse(text).apps[0].id).toBe("lobby");
});

test("rejects archives without a matching release.json", async () => {
  const missing = releaseArchive({ id: "r1", apps: [] }, { includeManifest: false });
  expect(await readTarFile(streamOf(missing.bytes), "release.json", 1024)).toBeUndefined();
  const other = releaseArchive({ id: "r2", apps: [] });
  expect(readManifest(streamOf(other.bytes), "r1")).rejects.toThrow("names release r2");
  expect(readTarFile(streamOf(other.bytes), "release.json", 10)).rejects.toThrow("larger than");
});
