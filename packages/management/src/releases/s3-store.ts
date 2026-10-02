import { createHash } from "node:crypto";

import type { ReleaseBucket } from "../config.ts";
import { bucketClient, s3Request, s3Stream } from "../s3.ts";
import type { ExpectedArchive, ReleaseStore } from "./store.ts";

/**
 * Stores archives in an S3-compatible bucket: stored ones below `<prefix>archives/`, uploads below `<prefix>uploads/`.
 * Clients upload and machines download through presigned URLs, which can't bind an upload to its digest, so an upload
 * is verified where it lands, then copied into place through a check of its size and digest. Uploads that are never
 * completed stay; a lifecycle rule on `<prefix>uploads/` can expire them.
 */
export function s3ReleaseStore(bucket: ReleaseBucket): ReleaseStore {
  const client = bucketClient(bucket);
  // Presigning is local, so this client only signs URLs for the address clients reach the bucket at.
  const publicClient = bucketClient({ ...bucket, endpoint: bucket.publicEndpoint });
  const archive = (key: string) => `${bucket.prefix}archives/${key}`;
  const upload = (key: string) => `${bucket.prefix}uploads/${key}`;
  const exists = (path: string) => s3Request("release store lookup", () => client.exists(path));
  const read = (path: string) => s3Stream("release store read", client.file(path));

  return {
    async uploadTarget(key, _expected, expireTime) {
      return {
        url: publicClient.presign(upload(key), { method: "PUT", expiresIn: secondsUntil(expireTime) }),
        method: "PUT",
        headers: { "content-type": "application/gzip" },
      };
    },

    async downloadUrl(key, expireTime) {
      return client.presign(archive(key), { expiresIn: secondsUntil(expireTime) });
    },

    exists: (key) => exists(archive(key)),

    async complete(key, expected, verify) {
      if (!(await exists(upload(key)))) {
        return (await exists(archive(key))) ? verify(read(archive(key))) : undefined;
      }
      const result = await verify(read(upload(key)));
      // The upload may have been replaced since, so only bytes that match the digest verified above are copied.
      const checked = matching(read(upload(key)), expected);
      await s3Request("release store copy", () =>
        client.write(archive(key), new Response(checked), { type: "application/gzip" }),
      );
      await s3Request("release store cleanup", () => client.delete(upload(key))).catch((error: unknown) =>
        console.error("removing a stored release's upload failed:", error),
      );
      return result;
    },
  };
}

/** `stream`, erroring before it ends unless it has the expected size and digest. */
function matching(stream: ReadableStream<Uint8Array>, expected: ExpectedArchive): ReadableStream<Uint8Array> {
  const hash = createHash("sha256");
  let size = 0n;
  return stream.pipeThrough(
    new TransformStream({
      transform(chunk, controller) {
        size += BigInt(chunk.byteLength);
        if (size > expected.sizeBytes) throw new Error("the archive is larger than declared");
        hash.update(chunk);
        controller.enqueue(chunk);
      },
      flush() {
        if (size !== expected.sizeBytes || hash.digest("hex") !== expected.sha256) {
          throw new Error("the archive does not match the declared size and digest");
        }
      },
    }),
  );
}

function secondsUntil(time: Date): number {
  return Math.max(1, Math.ceil((time.getTime() - Date.now()) / 1000));
}
