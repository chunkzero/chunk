import { untilAborted } from "./abort.ts";

/** Where an S3-compatible bucket is and the credentials that reach it. */
export interface Bucket {
  endpoint: string;
  region: string;
  bucket: string;
  accessKeyId: string;
  secretAccessKey: string;
  /** Empty for long-lived credentials. */
  sessionToken?: string;
}

/** The bucket is addressed by path, as core addresses it, which AWS S3, MinIO and R2 all accept. */
export function bucketClient({ endpoint, region, bucket, accessKeyId, secretAccessKey, sessionToken }: Bucket) {
  return new Bun.S3Client({
    endpoint,
    region,
    bucket,
    accessKeyId,
    secretAccessKey,
    ...(sessionToken ? { sessionToken } : {}),
  });
}

/**
 * Runs one request, failing with only the error's code, since a response can echo the request's credentials. With a
 * signal, it fails once the signal aborts, even if the request goes on, and a response that arrives later is ignored.
 */
export async function s3Request<T>(what: string, call: () => Promise<T>, signal?: AbortSignal): Promise<T> {
  signal?.throwIfAborted();
  const request = call().catch((error: unknown) => {
    throw s3Error(what, error);
  });
  if (!signal) return request;
  const result = await untilAborted(signal, request);
  signal.throwIfAborted();
  return result;
}

/** Only the error's code is kept: the rest may carry the response. */
function s3Error(what: string, error: unknown): Error {
  const code = (error as { code?: unknown } | undefined)?.code;
  const known = typeof code === "string" && /^\w+$/.test(code) ? `: ${code}` : "";
  return new Error(`${what} failed${known}`);
}
