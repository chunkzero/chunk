/** Globals available in the transactional_v1 profile (use lib ES2023, without DOM). */
interface TextEncoder {
  readonly encoding: "utf-8"
  encode(input?: string): Uint8Array<ArrayBuffer>
  encodeInto(source: string, destination: Uint8Array): { read: number; written: number }
}
declare const TextEncoder: { new(): TextEncoder }
interface TextDecoder {
  readonly encoding: string
  readonly fatal: boolean
  readonly ignoreBOM: boolean
  decode(input?: ArrayBuffer | ArrayBufferView, options?: { stream?: boolean }): string
}
declare const TextDecoder: { new(label?: string, options?: { fatal?: boolean; ignoreBOM?: boolean }): TextDecoder }
interface URLSearchParams extends Iterable<[string, string]> {
  readonly size: number
  append(name: string, value: string): void
  delete(name: string, value?: string): void
  get(name: string): string | null
  getAll(name: string): string[]
  has(name: string, value?: string): boolean
  set(name: string, value: string): void
  sort(): void
  entries(): IterableIterator<[string, string]>
  keys(): IterableIterator<string>
  values(): IterableIterator<string>
  forEach(callback: (value: string, key: string, params: URLSearchParams) => void, thisArg?: unknown): void
  toString(): string
}
declare const URLSearchParams: { new(init?: string | Iterable<readonly [string, string]> | Record<string, string>): URLSearchParams }
interface URL {
  href: string
  readonly origin: string
  protocol: string
  username: string
  password: string
  host: string
  hostname: string
  port: string
  pathname: string
  search: string
  readonly searchParams: URLSearchParams
  hash: string
  toString(): string
  toJSON(): string
}
declare const URL: {
  new(url: string | URL, base?: string | URL): URL
  canParse(url: string | URL, base?: string | URL): boolean
  parse(url: string | URL, base?: string | URL): URL | null
}
declare function atob(value: string): string
declare function btoa(value: string): string
declare function structuredClone<T>(value: T, options?: { transfer?: readonly never[] }): T
declare const console: { [K in "debug" | "log" | "info" | "warn" | "error"]: (...values: unknown[]) => void }
declare const crypto: {
  randomUUID(): string
  getRandomValues<T extends Int8Array | Uint8Array | Uint8ClampedArray | Int16Array | Uint16Array | Int32Array | Uint32Array | BigInt64Array | BigUint64Array>(array: T): T
  readonly subtle: { digest(algorithm: string | { name: string }, input: ArrayBuffer | ArrayBufferView): Promise<ArrayBuffer> }
}
