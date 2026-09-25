import { createHash } from "node:crypto";
import { mkdir, open, rename, rm } from "node:fs/promises";
import { dirname, join } from "node:path";

import { type Keys, randomToken } from "../crypto.ts";
import type { ReleaseStore } from "./store.ts";

const uploadPath = "/releases/upload/";
const keyPattern = /^[A-Za-z0-9_-]+\/[A-Za-z0-9_-]+\.tar\.gz$/;

/**
 * Stores archives in a local directory. Uploads go to this service through URLs signed for one archive's digest and
 * size, so they need no bearer token, and bytes that do not match are never stored.
 */
export function localReleaseStore({
  directory,
  keys,
  publicUrl,
}: {
  directory: string;
  keys: Keys;
  publicUrl: string;
}): ReleaseStore {
  const signed = (key: string, sha256: string, size: string, expires: string) =>
    ["upload", key, sha256, size, expires].join("\n");

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

    async read(key) {
      const file = Bun.file(join(directory, key));
      return (await file.exists()) ? file.stream() : undefined;
    },

    async fetch(request) {
      const url = new URL(request.url);
      if (!url.pathname.startsWith(uploadPath)) return undefined;
      if (request.method !== "PUT") return new Response("method not allowed\n", { status: 405 });
      const key = url.pathname.slice(uploadPath.length);
      const [sha256, size, expires, signature] = ["sha256", "size", "expires", "signature"].map(
        (name) => url.searchParams.get(name) ?? "",
      );
      if (
        !keyPattern.test(key) ||
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

      const path = join(directory, key);
      await mkdir(dirname(path), { recursive: true });
      const partial = `${path}.${randomToken(8)}.partial`;
      const file = await open(partial, "w");
      const hash = createHash("sha256");
      const expected = BigInt(size);
      let received = 0n;
      let stored = false;
      try {
        for await (const chunk of request.body) {
          received += BigInt(chunk.byteLength);
          if (received > expected) return new Response("archive is larger than declared\n", { status: 413 });
          hash.update(chunk);
          await file.write(chunk);
        }
        if (received !== expected || hash.digest("hex") !== sha256) {
          return new Response("archive does not match its declared size and digest\n", { status: 400 });
        }
        await file.sync();
        await file.close();
        await rename(partial, path);
        stored = true;
        return new Response(null, { status: 204 });
      } finally {
        if (!stored) {
          await file.close().catch(() => {});
          await rm(partial, { force: true });
        }
      }
    },
  };
}
