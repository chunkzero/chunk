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

/** Holds release archives, keyed by `releaseKey`. */
export interface ReleaseStore {
  /** Where a client sends an archive's bytes, such as a presigned URL. */
  uploadTarget(key: string, expected: ExpectedArchive, expireTime: Date): Promise<UploadTarget>;
  /** The stored archive, or undefined when nothing was uploaded under the key. */
  read(key: string): Promise<ReadableStream<Uint8Array> | undefined>;
  /**
   * Serves requests to URLs the store handed out that point at this service. Returns undefined for requests that are
   * not the store's.
   */
  fetch?(request: Request): Promise<Response | undefined>;
}

export function releaseKey(projectId: string, releaseId: string): string {
  return `${projectId}/${releaseId}.tar.gz`;
}
