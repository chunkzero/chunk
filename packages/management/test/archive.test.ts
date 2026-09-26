import { expect, test } from "bun:test";

import { type ArchiveLimits, scanArchive } from "../src/releases/archive.ts";
import { keepLimit, verifyRelease } from "../src/releases/manifest.ts";
import { type ArchiveOptions, rawArchive, releaseArchive } from "./fixtures.ts";

const limits: ArchiveLimits = { maxExpandedBytes: 64 * 1024 * 1024, maxEntries: 1000 };

function verify(archive: ReturnType<typeof releaseArchive>, overrides: Partial<ArchiveLimits> = {}, id = "r1") {
  return verifyRelease(new Blob([archive.bytes]).stream(), { releaseId: id, ...archive }, { ...limits, ...overrides });
}

function broken(options: ArchiveOptions) {
  return verify(releaseArchive("r1", undefined, options));
}

test("reads the apps and profiles chunk build writes", async () => {
  const manifest = await verify(releaseArchive("r1", [{ id: "lobby", sessions: ["default", "duel"] }]));
  expect(manifest).toEqual({
    id: "r1",
    apps: [
      {
        id: "lobby",
        sessions: {
          default: { machine_profile: "default", capacity: 16 },
          duel: { machine_profile: "default", capacity: 16 },
        },
      },
    ],
    profiles: { default: { memory_mib: 1024, max_sessions: 16 } },
  });
});

test("stores releases without checking what the environment validates", async () => {
  const unchecked = broken({
    omit: (path) => path.startsWith("apps/"),
    replace: { "backend.json": "not JSON", "contract.json": '{"contract_version":2.0}' },
    manifest: (manifest) => ({ ...manifest, java_version: "anything", assets: { missing: "0" } }),
  });
  expect((await unchecked).id).toBe("r1");
});

test("rejects release.json management cannot read", async () => {
  await expect(broken({ badChecksum: true })).rejects.toThrow("checksum");
  await expect(broken({ omit: (path) => path === "release.json" })).rejects.toThrow("no release.json");
  await expect(verify(rawArchive([["release.json", "not JSON"]]))).rejects.toThrow("is not UTF-8 JSON");
  await expect(verify(rawArchive([["release.json", '{"id":"r1","apps":[]}']]))).rejects.toThrow("is not version 3");
  await expect(broken({ manifest: (manifest) => ({ ...manifest, id: "r2" }) })).rejects.toThrow("names release r2");
  await expect(verify(releaseArchive("r1"), {}, "r9")).rejects.toThrow("names release r1");
  const profile = (value: unknown) =>
    broken({ manifest: (manifest) => ({ ...manifest, profiles: { default: value } }) });
  await expect(profile({ memory_mib: 1024.5, max_sessions: 16 })).rejects.toThrow("invalid profile default");
  await expect(profile({ memory_mib: 1024 })).rejects.toThrow("invalid profile default");
  const apps = releaseArchive("r1", [
    { id: "lobby", sessions: ["default"] },
    { id: "lobby", sessions: ["default"] },
  ]);
  await expect(verify(apps)).rejects.toThrow("lists app lobby twice");
  await expect(verify(releaseArchive("r1", [{ id: "lobby-1", sessions: ["default"] }]))).rejects.toThrow("invalid app");
});

test("keeps only release.json and reads names like Object properties as plain keys", async () => {
  expect(keepLimit("release.json")).toBe(16 * 1024 * 1024);
  for (const path of ["backend.json", "constructor", "__proto__", "toString", "hasOwnProperty"])
    expect(keepLimit(path)).toBeUndefined();
  const archive = releaseArchive("r1", [{ id: "lobby", sessions: ["__proto__"] }], {
    extra: [["constructor", "x".repeat(1024)]],
  });
  const manifest = await verify(archive);
  expect(Object.hasOwn(manifest.apps[0]?.sessions ?? {}, "__proto__")).toBe(true);
  expect(Object.getPrototypeOf(manifest.apps[0]?.sessions)).toBe(Object.prototype);
});

test("rejects a path that is both a file and a directory, in either order", async () => {
  const scan = (files: [string, string][]) =>
    scanArchive(new Blob([rawArchive(files).bytes]).stream(), limits, () => undefined);
  await expect(
    scan([
      ["assets", "x"],
      ["assets/file.txt", "y"],
    ]),
  ).rejects.toThrow("both a file and a directory");
  await expect(
    scan([
      ["assets/file.txt", "y"],
      ["assets", "x"],
    ]),
  ).rejects.toThrow("both a file and a directory");
  await expect(
    scan([
      ["assets/a/b.txt", "y"],
      ["assets/a", "x"],
    ]),
  ).rejects.toThrow("both a file and a directory");
});

test("rejects archives that differ from their declaration or exceed the budget", async () => {
  const archive = releaseArchive("r1");
  await expect(verify({ ...archive, sha256: "0".repeat(64) })).rejects.toThrow("declared size and digest");
  await expect(verify({ ...archive, sizeBytes: archive.sizeBytes - 1n })).rejects.toThrow("larger than declared");
  await expect(verify(archive, { maxExpandedBytes: 64 * 1024 })).rejects.toThrow("expands past 65536 bytes");
  await expect(verify(archive, { maxEntries: 3 })).rejects.toThrow("more than 3 entries");
});
