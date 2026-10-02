export const maxArchiveBytes = 2n * 1024n * 1024n * 1024n;

export interface ExpectedObject {
  /** Lowercase hex SHA-256. */
  sha256: string;
  sizeBytes: bigint;
}

export interface UploadTarget {
  url: string;
  method: string;
  headers: Record<string, string>;
}

/** Who fetches a download URL: environments' machines, or clients such as the CLI and players' game clients. */
export type Audience = "machines" | "clients";

/**
 * Holds release archives, keyed by `releaseKey`, and asset blobs, keyed by `blobKey`. Bytes that don't match an
 * object's declared size and digest are never stored under its key, and a stored object is never replaced by other
 * bytes.
 */
export interface ReleaseStore {
  /** Where a client sends an object's bytes, such as a presigned URL. */
  uploadTarget(key: string, expected: ExpectedObject, expireTime: Date): Promise<UploadTarget>;
  /** A URL `audience`, machines unless set, GETs the stored object from without other credentials. */
  downloadUrl(key: string, expireTime: Date, audience?: Audience): Promise<string>;
  /** Answers a GET of the stored object for `audience`: its bytes, or a redirect to a download URL. */
  serve(key: string, audience: Audience): Promise<Response>;
  /** Whether an object is stored under the key. */
  exists(key: string): Promise<boolean>;
  /**
   * Streams the object uploaded under the key, or else the one stored there, through `verify`, and stores an uploaded
   * one that matches `expected`. Resolves to what `verify` resolves to, or undefined when there is no object; rejects
   * when `verify` does, or when the object does not match.
   */
  complete<T>(
    key: string,
    expected: ExpectedObject,
    verify: (object: ReadableStream<Uint8Array>) => Promise<T>,
  ): Promise<T | undefined>;
  /**
   * Serves requests to URLs the store handed out that point at this service. Returns undefined for requests that are
   * not the store's.
   */
  fetch?(request: Request): Promise<Response | undefined>;
}

/** Keyed by digest, so an upload of other bytes, for example through an old URL, never replaces a verified archive. */
export function releaseKey(projectId: string, releaseId: string, sha256: string): string {
  return `${projectId}/${releaseId}/${sha256}.tar.gz`;
}

/** An asset blob, stored once per project. */
export function blobKey(projectId: string, sha256: string): string {
  return `${projectId}/blobs/${sha256}`;
}

export function contentType(key: string): string {
  return key.endsWith(".tar.gz") ? "application/gzip" : "application/octet-stream";
}

/** How long a URL `serve` redirects to stays valid. */
export const servedUrlLifetimeMs = 15 * 60 * 1000;
