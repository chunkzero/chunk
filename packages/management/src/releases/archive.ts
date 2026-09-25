const blockSize = 512;
const maxLongNameBytes = 4096;

/**
 * Reads one regular file from a gzip-compressed tar archive, stopping once it is found. Returns undefined when the
 * archive does not hold the file; throws when the archive is malformed or the file exceeds `maxBytes`.
 */
export async function readTarFile(
  archive: ReadableStream<ArrayBufferView | ArrayBuffer>,
  name: string,
  maxBytes: number,
): Promise<Uint8Array | undefined> {
  const reader = archive.pipeThrough(new DecompressionStream("gzip")).getReader();
  const bytes = new ByteReader(reader);
  try {
    let longName: string | undefined;
    for (;;) {
      const header = await bytes.take(blockSize);
      if (!header || header.every((byte) => byte === 0)) return undefined;
      const size = entrySize(header.subarray(124, 136));
      const padded = Math.ceil(size / blockSize) * blockSize;
      const type = header[156];
      if (type === 0x4c) {
        // GNU long name: the entry's data is the next entry's path.
        if (size > maxLongNameBytes) throw new Error("tar long name is too long");
        const data = await bytes.take(padded);
        if (!data) throw new Error("truncated tar archive");
        longName = text(data.subarray(0, size));
        continue;
      }
      const prefix = text(header.subarray(257, 263)) === "ustar" ? text(header.subarray(345, 500)) : "";
      const path = (longName ?? (prefix ? `${prefix}/` : "") + text(header.subarray(0, 100))).replace(/^\.\//, "");
      longName = undefined;
      if (path === name && (type === 0x30 || type === 0)) {
        if (size > maxBytes) throw new Error(`${name} is larger than ${maxBytes} bytes`);
        const data = await bytes.take(padded);
        if (!data) throw new Error("truncated tar archive");
        return data.subarray(0, size);
      }
      if (!(await bytes.skip(padded))) throw new Error("truncated tar archive");
    }
  } finally {
    await reader.cancel().catch(() => {});
  }
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
  return octal ? Number.parseInt(octal, 8) : 0;
}

class ByteReader {
  private chunks: Uint8Array[] = [];
  private buffered = 0;

  constructor(private readonly reader: { read(): Promise<{ done: boolean; value?: Uint8Array | undefined }> }) {}

  /** The next `count` bytes, or undefined if the stream ends first. */
  async take(count: number): Promise<Uint8Array | undefined> {
    if (!(await this.fill(count))) return undefined;
    const out = new Uint8Array(count);
    let offset = 0;
    while (offset < count) {
      offset += this.consume(count - offset, (chunk) => out.set(chunk, offset));
    }
    return out;
  }

  /** Discards the next `count` bytes; false if the stream ends first. */
  async skip(count: number): Promise<boolean> {
    let remaining = count;
    while (remaining > 0) {
      if (!(await this.fill(1))) return false;
      remaining -= this.consume(remaining, () => {});
    }
    return true;
  }

  private consume(limit: number, use: (chunk: Uint8Array) => void): number {
    const chunk = this.chunks[0];
    if (!chunk) return 0;
    const count = Math.min(chunk.byteLength, limit);
    use(chunk.subarray(0, count));
    if (count === chunk.byteLength) this.chunks.shift();
    else this.chunks[0] = chunk.subarray(count);
    this.buffered -= count;
    return count;
  }

  private async fill(count: number): Promise<boolean> {
    while (this.buffered < count) {
      const { done, value } = await this.reader.read();
      if (done || !value) return false;
      this.chunks.push(value);
      this.buffered += value.byteLength;
    }
    return true;
  }
}
