export const maxArchiveBytes = 2n * 1024n * 1024n * 1024n;

export interface ExpectedArchive {
  /** Lowercase hex SHA-256. */
  sha256: string;
  sizeBytes: bigint;
}

export interface UploadTarget {
  url: string;
  method: string;
  headers: Record<string, string>;
}

/**
 * Holds release archives, keyed by `releaseKey`. Bytes that don't match an archive's declared size and digest are never
 * stored under its key, and a stored archive is never replaced by other bytes.
 */
export interface ReleaseStore {
  /** Where a client sends an archive's bytes, such as a presigned URL. */
  uploadTarget(key: string, expected: ExpectedArchive, expireTime: Date): Promise<UploadTarget>;
  /** A URL environments GET the stored archive from without other credentials. */
  downloadUrl(key: string, expireTime: Date): Promise<string>;
  /** Whether an archive is stored under the key. */
  exists(key: string): Promise<boolean>;
  /**
   * Streams the archive uploaded under the key, or else the one stored there, through `verify`, and stores an uploaded
   * one that matches `expected`. Resolves to what `verify` resolves to, or undefined when there is no archive; rejects
   * when `verify` does, or when the archive does not match.
   */
  complete<T>(
    key: string,
    expected: ExpectedArchive,
    verify: (archive: ReadableStream<Uint8Array>) => Promise<T>,
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
