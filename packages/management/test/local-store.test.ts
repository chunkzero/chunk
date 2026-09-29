import { expect, spyOn, test } from "bun:test";
import { createHash, randomBytes } from "node:crypto";
import * as fs from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

import { deriveKeys } from "../src/crypto.ts";
import { localReleaseStore } from "../src/releases/local-store.ts";
import { releaseKey } from "../src/releases/store.ts";

test("a start syncs the whole store path after an interrupted one, and an upload syncs after the archive", async () => {
  const base = await fs.mkdtemp(join(tmpdir(), "chunk-local-store-"));
  const synced: string[] = [];
  let failSync = false;
  const open = fs.open;
  const opening = spyOn(fs, "open").mockImplementation(async (...args) => {
    const handle = await open(...args);
    const sync = handle.sync.bind(handle);
    handle.sync = () => {
      if (failSync) return Promise.reject(new Error("interrupted"));
      synced.push(String(args[0]));
      return sync();
    };
    return handle;
  });
  try {
    const root = join(base, "data", "releases");
    const start = () =>
      localReleaseStore({
        directory: root,
        keys: deriveKeys(randomBytes(32)),
        publicUrl: "http://x",
        machineUrl: "http://x",
      });
    failSync = true;
    await expect(start()).rejects.toThrow("interrupted");
    failSync = false;
    expect(await fs.stat(root)).toBeDefined();

    const store = await start();
    const ancestry = [root];
    for (let dir = root; dir !== dirname(dir);) ancestry.push((dir = dirname(dir)));
    expect(synced).toEqual(ancestry);

    synced.length = 0;
    const bytes = new TextEncoder().encode("archive");
    const sha256 = createHash("sha256").update(bytes).digest("hex");
    const expireTime = new Date(Date.now() + 60_000);
    const target = await store.uploadTarget(
      releaseKey("p", "r", sha256),
      { sha256, sizeBytes: BigInt(bytes.byteLength) },
      expireTime,
    );
    const response = await store.fetch?.(new Request(target.url, { method: "PUT", body: bytes }));
    expect(response?.status).toBe(204);
    expect(synced).toEqual([expect.stringMatching(/\.partial$/), join(root, "p", "r"), join(root, "p"), root]);
  } finally {
    opening.mockRestore();
    await fs.rm(base, { recursive: true, force: true });
  }
});

test("uploads use the public URL and downloads the URL machines reach this service at", async () => {
  const root = await fs.mkdtemp(join(tmpdir(), "chunk-local-store-"));
  try {
    const store = await localReleaseStore({
      directory: root,
      keys: deriveKeys(randomBytes(32)),
      publicUrl: "http://localhost:8080",
      machineUrl: "http://management:8080",
    });
    const key = releaseKey("p", "r", "0".repeat(64));
    const expireTime = new Date(Date.now() + 60_000);
    const target = await store.uploadTarget(key, { sha256: "0".repeat(64), sizeBytes: 1n }, expireTime);
    expect(target.url).toStartWith("http://localhost:8080/");
    expect(await store.downloadUrl(key, expireTime)).toStartWith("http://management:8080/");
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});
