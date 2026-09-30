import type { LogStoreGrant } from "./issuer.ts";

/** How many objects are deleted at once. */
const deleteConcurrency = 16;

/**
 * Deletes every object below the grant's prefix, a listed page at a time until none are left, and stops once `signal`
 * aborts. The bucket is addressed by path, as core addresses it.
 */
export async function deletePrefix(grant: LogStoreGrant, signal: AbortSignal): Promise<void> {
  const client = new Bun.S3Client({
    endpoint: grant.endpoint,
    region: grant.region,
    bucket: grant.bucket,
    accessKeyId: grant.accessKeyId,
    secretAccessKey: grant.secretAccessKey,
    ...(grant.sessionToken ? { sessionToken: grant.sessionToken } : {}),
  });
  for (;;) {
    signal.throwIfAborted();
    const listed = await s3("listing", () => client.list({ prefix: grant.prefix }));
    const keys = (listed.contents ?? []).map(({ key }) => key);
    // An empty or final page is trusted only when the listing accounts for every key it counted.
    if (typeof listed.isTruncated !== "boolean" || listed.keyCount !== keys.length) {
      throw new Error("log store listing was incomplete");
    }
    if (keys.some((key) => !key.startsWith(grant.prefix))) throw new Error("log store listing returned foreign keys");
    if (keys.length === 0) {
      if (listed.isTruncated) throw new Error("log store listing was truncated without keys");
      return;
    }
    for (let i = 0; i < keys.length; i += deleteConcurrency) {
      signal.throwIfAborted();
      const batch = keys.slice(i, i + deleteConcurrency);
      await Promise.all(batch.map((key) => s3("deletion", () => client.delete(key))));
    }
  }
}

/** Runs one request, keeping only the error's code, since a response can echo the request's credentials. */
async function s3<T>(what: string, call: () => Promise<T>): Promise<T> {
  try {
    return await call();
  } catch (error) {
    const code = (error as { code?: unknown } | undefined)?.code;
    const known = typeof code === "string" && /^\w+$/.test(code) ? `: ${code}` : "";
    throw new Error(`log store ${what} failed${known}`);
  }
}
