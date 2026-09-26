import { expect, spyOn, test } from "bun:test";
import { createHash, randomBytes } from "node:crypto";
import * as fs from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { deriveKeys } from "../src/crypto.ts";
import { localReleaseStore } from "../src/releases/local-store.ts";
import { releaseKey } from "../src/releases/store.ts";

test("a first run syncs the directories it creates, and an upload syncs its directories after the archive", async () => {
  const base = await fs.mkdtemp(join(tmpdir(), "chunk-local-store-"));
  const synced: string[] = [];
  const open = fs.open;
  const opening = spyOn(fs, "open").mockImplementation(async (...args) => {
    const handle = await open(...args);
    const sync = handle.sync.bind(handle);
    handle.sync = () => {
      synced.push(String(args[0]));
      return sync();
    };
    return handle;
  });
  try {
    const root = join(base, "data", "releases");
    const store = await localReleaseStore({
      directory: root,
      keys: deriveKeys(randomBytes(32)),
      publicUrl: "http://x",
    });
    expect(synced).toEqual([root, join(base, "data"), base]);

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
