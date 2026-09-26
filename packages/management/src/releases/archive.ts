import { createHash } from "node:crypto";

const blockSize = 512;
const maxLongNameBytes = 4096;
/** The file and directory paths a scan retains, in UTF-16 code units, so deep paths cannot amplify memory. */
export const maxPathChars = 16 * 1024 * 1024;

export interface ArchiveLimits {
  /** Decompressed bytes, tar headers and padding included. */
  maxExpandedBytes: number;
  maxEntries: number;
}

export interface ArchiveEntry {
  size: number;
  /** Lowercase hex SHA-256 of the entry's contents. */
  sha256: string;
  /** The contents, for entries `keep` asked for. */
  data: Uint8Array | undefined;
}

/**
 * Reads a gzip-compressed tar archive to its end within `limits`, checking header checksums, entry types, paths and
 * the end-of-archive marker. Hashes every file and keeps the contents of those `keep` returns a byte limit for.
 * Throws with a reason when the archive is malformed or too large.
 */
export async function scanArchive(
  archive: ReadableStream<ArrayBufferView | ArrayBuffer>,
  limits: ArchiveLimits,
  keep: (path: string) => number | undefined,
): Promise<Map<string, ArchiveEntry>> {
  const reader = archive.pipeThrough(new DecompressionStream("gzip")).getReader();
  const bytes = new ByteReader(reader, limits.maxExpandedBytes);
  const entries = new Map<string, ArchiveEntry>();
  /** Every ancestor of a file seen so far; none of them may also be a file. */
  const directories = new Set<string>();
  let pathChars = 0;
  const retain = (path: string) => {
    pathChars += path.length;
    if (pathChars > maxPathChars) throw new Error(`the archive's paths exceed the ${maxPathChars}-character budget`);
  };
  try {
    let longName: string | undefined;
    let count = 0;
    for (;;) {
      const header = await bytes.take(blockSize);
      if (header.every((byte) => byte === 0)) {
        await bytes.drain((chunk) => {
          if (chunk.some((byte) => byte !== 0)) throw new Error("data follows the end of the tar archive");
        });
        if (longName !== undefined) throw new Error("tar long name has no entry");
        return entries;
      }
      if (++count > limits.maxEntries) throw new Error(`the archive has more than ${limits.maxEntries} entries`);
      if (!checksumMatches(header)) throw new Error("tar header checksum mismatch");
      const size = entrySize(header.subarray(124, 136));
      const padding = Math.ceil(size / blockSize) * blockSize - size;
      const type = header[156];
      if (type === 0x4c) {
        // GNU long name: the entry's data is the next entry's path.
        if (size > maxLongNameBytes) throw new Error("tar long name is too long");
        longName = text(await bytes.take(size));
        await bytes.skip(padding);
        continue;
      }
      if (type !== 0x30 && type !== 0) throw new Error("the archive holds something other than regular files");
      const prefix = text(header.subarray(257, 263)) === "ustar" ? text(header.subarray(345, 500)) : "";
      const path = longName ?? (prefix ? `${prefix}/` : "") + text(header.subarray(0, 100));
      longName = undefined;
      if (!portablePath(path)) throw new Error(`the archive holds an invalid path: ${JSON.stringify(path)}`);
      if (entries.has(path)) throw new Error(`the archive holds ${path} twice`);
      if (directories.has(path)) throw new Error(`the archive holds ${path} as both a file and a directory`);
      retain(path);
      // Deepest first: a known directory's ancestors are already known, so each directory is checked once.
      for (let slash = path.lastIndexOf("/"); slash > 0; slash = path.lastIndexOf("/", slash - 1)) {
        const ancestor = path.slice(0, slash);
        if (directories.has(ancestor)) break;
        if (entries.has(ancestor)) throw new Error(`the archive holds ${ancestor} as both a file and a directory`);
        retain(ancestor);
        directories.add(ancestor);
      }
      const limit = keep(path);
      if (limit !== undefined && size > limit) throw new Error(`${path} is larger than ${limit} bytes`);
      const hash = createHash("sha256");
      const kept: Uint8Array[] = [];
      await bytes.read(size, (chunk) => {
        hash.update(chunk);
        if (limit !== undefined) kept.push(chunk);
      });
      await bytes.skip(padding);
      entries.set(path, {
        size,
        sha256: hash.digest("hex"),
        data: limit === undefined ? undefined : Buffer.concat(kept),
      });
    }
  } finally {
    await reader.cancel().catch(() => {});
  }
}

function portablePath(path: string): boolean {
  return path.length > 0 && path.split("/").every((part) => part !== "" && part !== "." && part !== "..");
}

function checksumMatches(header: Uint8Array): boolean {
  let sum = 0;
  for (let i = 0; i < blockSize; i++) sum += i >= 148 && i < 156 ? 0x20 : (header[i] ?? 0);
  const stored = text(header.subarray(148, 156)).trim();
  return /^[0-7]+$/.test(stored) && Number.parseInt(stored, 8) === sum;
}

function text(bytes: Uint8Array): string {
  const end = bytes.indexOf(0);
  return new TextDecoder().decode(end === -1 ? bytes : bytes.subarray(0, end));
}

function entrySize(field: Uint8Array): number {
  const first = field[0] ?? 0;
  if (first & 0x80) {
    // GNU base-256 encoding for sizes past the octal field's range.
    return field.subarray(1).reduce((size, byte) => size * 256 + byte, first & 0x7f);
  }
  const octal = text(field).trim();
  if (!/^[0-7]*$/.test(octal)) throw new Error("tar entry size is not octal");
  return octal ? Number.parseInt(octal, 8) : 0;
}

class ByteReader {
  private chunks: Uint8Array[] = [];
  private buffered = 0;
  private total = 0;

  constructor(
    private readonly reader: { read(): Promise<{ done: boolean; value?: Uint8Array | undefined }> },
    private readonly maxBytes: number,
  ) {}

  /** The next `count` bytes; throws if the stream ends first. */
  async take(count: number): Promise<Uint8Array> {
    const out = new Uint8Array(count);
    let offset = 0;
    await this.read(count, (chunk) => {
      out.set(chunk, offset);
      offset += chunk.byteLength;
    });
    return out;
  }

  /** Passes the next `count` bytes to `use` in pieces; throws if the stream ends first. */
  async read(count: number, use: (chunk: Uint8Array) => void): Promise<void> {
    let remaining = count;
    while (remaining > 0) {
      if (this.buffered === 0 && !(await this.pull())) throw new Error("truncated tar archive");
      const chunk = this.chunks[0];
      if (!chunk) continue;
      const taken = Math.min(chunk.byteLength, remaining);
      use(chunk.subarray(0, taken));
      if (taken === chunk.byteLength) this.chunks.shift();
      else this.chunks[0] = chunk.subarray(taken);
      this.buffered -= taken;
      remaining -= taken;
    }
  }

  skip(count: number): Promise<void> {
    return this.read(count, () => {});
  }

  /** Passes everything left in the stream to `use`. */
  async drain(use: (chunk: Uint8Array) => void): Promise<void> {
    for (const chunk of this.chunks) use(chunk);
    this.chunks = [];
    this.buffered = 0;
    while (await this.pull()) {
      const chunk = this.chunks.pop();
      this.buffered = 0;
      if (chunk) use(chunk);
    }
  }

  private async pull(): Promise<boolean> {
    const { done, value } = await this.reader.read();
    if (done || !value) return false;
    this.total += value.byteLength;
    if (this.total > this.maxBytes) throw new Error(`the archive expands past ${this.maxBytes} bytes`);
    this.chunks.push(value);
    this.buffered += value.byteLength;
    return true;
  }
}
