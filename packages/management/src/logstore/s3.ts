import { bucketClient, s3Request } from "../s3.ts";
import type { LogStoreGrant } from "./issuer.ts";

/** How many objects are deleted at once. */
const deleteConcurrency = 16;

/**
 * Deletes every object below the grant's prefix, a listed page at a time until none are left, and stops once `signal`
 * aborts.
 */
export async function deletePrefix(grant: LogStoreGrant, signal: AbortSignal): Promise<void> {
  const client = bucketClient(grant);
  for (;;) {
    const listed = await s3Request("log store listing", () => client.list({ prefix: grant.prefix }), signal);
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
      const batch = keys.slice(i, i + deleteConcurrency);
      await s3Request("log store deletion", () => Promise.all(batch.map((key) => client.delete(key))), signal);
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
 * Every snapshot below the grant's prefix, newest first, listing a page at a time. Once `signal` aborts, the listing
 * fails, even when a page arrives after.
 */
export async function listSnapshots(grant: LogStoreGrant, signal: AbortSignal): Promise<StoredSnapshot[]> {
  const client = bucketClient(grant);
  const prefix = `${grant.prefix}epochs/`;
  const snapshots: StoredSnapshot[] = [];
  let continuationToken: string | undefined;
  do {
    const options = { prefix, ...(continuationToken ? { continuationToken } : {}) };
    const listed = await s3Request("log store listing", () => client.list(options), signal);
    for (const { key, lastModified } of listed.contents ?? []) {
      const match = key.startsWith(grant.prefix) ? snapshotKey.exec(key.slice(grant.prefix.length)) : null;
      if (!match?.[1] || !match[2]) continue;
      const createTime = lastModified ? new Date(lastModified) : undefined;
      snapshots.push({ epoch: BigInt(match[1]), sequence: BigInt(match[2]), createTime });
    }
    continuationToken = listed.isTruncated ? listed.nextContinuationToken : undefined;
    if (listed.isTruncated && !continuationToken) throw new Error("log store listing was truncated without a token");
  } while (continuationToken);
  return snapshots.sort((a, b) => (olderThan(a, b) ? 1 : olderThan(b, a) ? -1 : 0));
}
