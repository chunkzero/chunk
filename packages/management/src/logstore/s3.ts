import type { LogStoreGrant } from "./issuer.ts";
import { signV4 } from "./sigv4.ts";
import { xmlElement } from "./xml.ts";

/** How many objects are deleted at once. */
const deleteConcurrency = 16;

/**
 * Deletes every object below the grant's prefix, a listed page at a time until none are left, and stops once `signal`
 * aborts.
 */
export async function deletePrefix(grant: LogStoreGrant, signal: AbortSignal): Promise<void> {
  const client = clientOf(grant);
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

/** Where a snapshot sits in an environment's log. */
export interface SnapshotPosition {
  epoch: bigint;
  /** The last log sequence it contains. */
  sequence: bigint;
}

/** A snapshot stored in an environment's log. */
export interface StoredSnapshot extends SnapshotPosition {
  /** When it was stored. */
  createTime: Date | undefined;
}

/** Core's snapshot objects, relative to the log prefix; `chunk-store`'s replication module defines the layout. */
const snapshotKey = /^epochs\/(\d{20})\/snapshots\/(\d{20})\.db$/;

/** A snapshot's ID: `{epoch}-{sequence}` in decimal, as core parses it. */
export function snapshotId({ epoch, sequence }: SnapshotPosition): string {
  return `${epoch}-${sequence}`;
}

/** The position a snapshot ID names, or undefined for anything else. */
export function parseSnapshotId(id: string): SnapshotPosition | undefined {
  const match = /^(\d{1,20})-(\d{1,20})$/.exec(id);
  return match?.[1] && match[2] ? { epoch: BigInt(match[1]), sequence: BigInt(match[2]) } : undefined;
}

/** Whether `a` is older than `b`. */
export function olderThan(a: SnapshotPosition, b: SnapshotPosition): boolean {
  return a.epoch < b.epoch || (a.epoch === b.epoch && a.sequence < b.sequence);
}

/**
 * Every snapshot below the grant's prefix, newest first, listing a page at a time. Once `signal` aborts, the request
 * under way is cancelled and the listing fails, even when a page arrives after.
 */
export async function listSnapshots(grant: LogStoreGrant, signal: AbortSignal): Promise<StoredSnapshot[]> {
  const snapshots: StoredSnapshot[] = [];
  let continuationToken: string | undefined;
  do {
    const xml = await listPage(grant, `${grant.prefix}epochs/`, continuationToken, signal);
    for (const [, entry = ""] of xml.matchAll(/<Contents>([\s\S]*?)<\/Contents>/g)) {
      const key = xmlElement(entry, "Key") ?? "";
      const match = key.startsWith(grant.prefix) ? snapshotKey.exec(key.slice(grant.prefix.length)) : null;
      if (!match?.[1] || !match[2]) continue;
      const lastModified = xmlElement(entry, "LastModified");
      const createTime = lastModified ? new Date(lastModified) : undefined;
      snapshots.push({ epoch: BigInt(match[1]), sequence: BigInt(match[2]), createTime });
    }
    const truncated = xmlElement(xml, "IsTruncated") === "true";
    continuationToken = truncated ? xmlElement(xml, "NextContinuationToken") : undefined;
    if (truncated && !continuationToken) throw new Error("log store listing was truncated without a token");
  } while (continuationToken);
  return snapshots.sort((a, b) => (olderThan(a, b) ? 1 : olderThan(b, a) ? -1 : 0));
}

/** One ListObjectsV2 page of the keys below `prefix`, as XML, addressing the bucket by path as core does. */
async function listPage(
  grant: LogStoreGrant,
  prefix: string,
  continuationToken: string | undefined,
  signal: AbortSignal,
): Promise<string> {
  const url = new URL(`${grant.endpoint.replace(/\/$/, "")}/${grant.bucket}`);
  url.searchParams.set("list-type", "2");
  url.searchParams.set("prefix", prefix);
  if (continuationToken) url.searchParams.set("continuation-token", continuationToken);
  const headers = signV4(
    {
      method: "GET",
      url,
      headers: {
        "x-amz-content-sha256": emptySha256,
        ...(grant.sessionToken ? { "x-amz-security-token": grant.sessionToken } : {}),
      },
    },
    { accessKeyId: grant.accessKeyId, secretAccessKey: grant.secretAccessKey, region: grant.region, service: "s3" },
  );
  const response = await fetch(url, { headers, signal });
  const xml = await response.text();
  signal.throwIfAborted();
  if (!response.ok) {
    // Only the error's code: a response can echo the request.
    const code = /^\w+$/.exec(xmlElement(xml, "Code") ?? "")?.[0];
    throw new Error(`log store listing failed with HTTP ${response.status}${code ? `: ${code}` : ""}`);
  }
  return xml;
}

const emptySha256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/** The bucket is addressed by path, as core addresses it. */
function clientOf(grant: LogStoreGrant): Bun.S3Client {
  return new Bun.S3Client({
    endpoint: grant.endpoint,
    region: grant.region,
    bucket: grant.bucket,
    accessKeyId: grant.accessKeyId,
    secretAccessKey: grant.secretAccessKey,
    ...(grant.sessionToken ? { sessionToken: grant.sessionToken } : {}),
  });
}

/** Runs one request, keeping only the error's code, since a response can echo the request's credentials. */
async function s3<T>(what: string, call: () => Promise<T>): Promise<T> {
  try {
    return await call();
  } catch (error) {
    const code = (error as { code?: unknown } | undefined)?.code;
    const known = typeof code === "string" && /^\w+$/.test(code) ? `: ${code}` : "";
    // The cause is left out on purpose: it may carry the response.
    // oxlint-disable-next-line preserve-caught-error
    throw new Error(`log store ${what} failed${known}`);
  }
}
