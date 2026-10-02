import { createHash } from "node:crypto";
import { link, mkdir, open, rm } from "node:fs/promises";
import { dirname, join, parse, resolve } from "node:path";

import { type Keys, randomToken } from "../crypto.ts";
import type { ReleaseStore } from "./store.ts";

const uploadPath = "/releases/upload/";
const downloadPath = "/releases/download/";
const keyPattern = /^[A-Za-z0-9_-]+\/[A-Za-z0-9_-]+\/([0-9a-f]{64})\.tar\.gz$/;

/** Flushes the entries of `from` and each directory above it, up to and including `to`. */
async function syncDirectories(from: string, to: string) {
  for (let dir = from; ; dir = dirname(dir)) {
    const handle = await open(dir, "r");
    try {
      await handle.sync();
    } finally {
      await handle.close();
    }
    if (dir === to || dir === dirname(dir)) return;
  }
}

/**
 * Stores archives in a local directory, which it creates durably first. Uploads go to this service through URLs signed
 * for one archive's key, digest and size, so they need no bearer token. Bytes that do not match are never stored, and
 * a stored object is never replaced.
 */
export async function localReleaseStore({
  directory,
  keys,
  publicUrl,
  machineUrl,
}: {
  directory: string;
  keys: Keys;
  publicUrl: string;
  /** Where machines reach this service; download URLs only go to machines. */
  machineUrl: string;
}): Promise<ReleaseStore> {
  const root = resolve(directory);
  await mkdir(root, { recursive: true });
  // Every start, since an earlier one may have created directories and stopped before syncing their entries.
  await syncDirectories(root, parse(root).root);
  const signed = (key: string, sha256: string, size: string, expires: string) =>
    ["upload", key, sha256, size, expires].join("\n");

  async function stored(key: string) {
    const path = join(root, key);
    const file = Bun.file(path);
    if (!(await file.exists())) return undefined;
    // A concurrent upload may have linked the archive without syncing its directories yet.
    await syncDirectories(dirname(path), root);
    return file;
  }

  async function download(request: Request, url: URL): Promise<Response> {
    if (request.method !== "GET" && request.method !== "HEAD") {
      return new Response("method not allowed\n", { status: 405 });
    }
    const key = url.pathname.slice(downloadPath.length);
    const expires = url.searchParams.get("expires") ?? "";
    const signature = url.searchParams.get("signature") ?? "";
    if (!keyPattern.test(key) || !keys.verify(["download", key, expires].join("\n"), signature)) {
      return new Response("invalid download signature\n", { status: 403 });
    }
    if (Number(expires) * 1000 < Date.now()) return new Response("download URL expired\n", { status: 403 });
    const file = Bun.file(join(directory, key));
    if (!(await file.exists())) return new Response("not found\n", { status: 404 });
    return new Response(file, { headers: { "content-type": "application/gzip" } });
  }

  return {
    async uploadTarget(key, { sha256, sizeBytes }, expireTime) {
      const expires = Math.floor(expireTime.getTime() / 1000).toString();
      const query = new URLSearchParams({
        sha256,
        size: sizeBytes.toString(),
        expires,
        signature: keys.sign(signed(key, sha256, sizeBytes.toString(), expires)),
      });
      return {
        url: `${publicUrl}${uploadPath}${key}?${query}`,
        method: "PUT",
        headers: { "content-type": "application/gzip" },
      };
    },

    async downloadUrl(key, expireTime) {
      const expires = Math.floor(expireTime.getTime() / 1000).toString();
      const signature = keys.sign(["download", key, expires].join("\n"));
      return `${machineUrl}${downloadPath}${key}?${new URLSearchParams({ expires, signature })}`;
    },

    exists: async (key) => (await stored(key)) !== undefined,

    async complete(key, _expected, verify) {
      // Uploads were checked against the key's digest as they were stored, so the stored archive is the upload.
      const file = await stored(key);
      return file && verify(file.stream());
    },

    async fetch(request) {
      const url = new URL(request.url);
      if (url.pathname.startsWith(downloadPath)) return download(request, url);
      if (!url.pathname.startsWith(uploadPath)) return undefined;
      if (request.method !== "PUT") return new Response("method not allowed\n", { status: 405 });
      const key = url.pathname.slice(uploadPath.length);
      const [sha256, size, expires, signature] = ["sha256", "size", "expires", "signature"].map(
        (name) => url.searchParams.get(name) ?? "",
      );
      if (
        keyPattern.exec(key)?.[1] !== sha256 ||
        !sha256 ||
        !size ||
        !expires ||
        !signature ||
        !keys.verify(signed(key, sha256, size, expires), signature)
      ) {
        return new Response("invalid upload signature\n", { status: 403 });
      }
      if (Number(expires) * 1000 < Date.now()) return new Response("upload URL expired\n", { status: 403 });
      if (!request.body) return new Response("missing body\n", { status: 400 });

      const path = join(root, key);
      // Only below the root, so uploads fail instead of recreating a deleted root that was never synced.
      for (const dir of [dirname(dirname(path)), dirname(path)]) {
        await mkdir(dir).catch((error: NodeJS.ErrnoException) => {
          if (error.code !== "EEXIST") throw error;
        });
      }
      const partial = `${path}.${randomToken(8)}.partial`;
      const file = await open(partial, "w");
      const hash = createHash("sha256");
      const expected = BigInt(size);
      let received = 0n;
      try {
        for await (const chunk of request.body) {
          received += BigInt(chunk.byteLength);
          if (received > expected) return new Response("archive is larger than declared\n", { status: 413 });
          hash.update(chunk);
          for (let offset = 0; offset < chunk.byteLength;) {
            const { bytesWritten } = await file.write(chunk, offset, chunk.byteLength - offset);
            if (bytesWritten <= 0) throw new Error(`short write to ${partial}`);
            offset += bytesWritten;
          }
        }
        if (received !== expected || hash.digest("hex") !== sha256) {
          return new Response("archive does not match its declared size and digest\n", { status: 400 });
        }
        await file.sync();
        await file.close();
        // The key names the digest, so an object already stored under it holds these same bytes.
        await link(partial, path).catch((error: NodeJS.ErrnoException) => {
          if (error.code !== "EEXIST") throw error;
        });
        await syncDirectories(dirname(path), root);
        return new Response(null, { status: 204 });
      } finally {
        await file.close().catch(() => {});
        await rm(partial, { force: true });
      }
    },
  };
}
