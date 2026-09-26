import { createHash, createHmac } from "node:crypto";

export interface SigningKey {
  accessKeyId: string;
  secretAccessKey: string;
  region: string;
  service: string;
}

/**
 * The headers that sign a request with AWS Signature Version 4: `host`, `x-amz-date` and `authorization`, plus the
 * given ones, which are all signed.
 */
export function signV4(
  {
    method,
    url,
    headers = {},
    body = "",
  }: { method: string; url: URL; headers?: Record<string, string>; body?: string },
  key: SigningKey,
  now = new Date(),
): Record<string, string> {
  const amzDate = now
    .toISOString()
    .replace(/[-:]/g, "")
    .replace(/\.\d{3}/, "");
  const date = amzDate.slice(0, 8);
  const signed: Record<string, string> = { ...lowercase(headers), host: url.host, "x-amz-date": amzDate };
  const names = Object.keys(signed).sort();
  const query = [...url.searchParams]
    .map(([name, value]) => [encode(name), encode(value)])
    .sort(([a = "", x = ""], [b = "", y = ""]) => (a === b ? compare(x, y) : compare(a, b)))
    .map(([name, value]) => `${name}=${value}`)
    .join("&");
  const canonical = [
    method,
    url.pathname || "/",
    query,
    names.map((name) => `${name}:${signed[name]?.trim()}\n`).join(""),
    names.join(";"),
    hex(body),
  ].join("\n");
  const scope = `${date}/${key.region}/${key.service}/aws4_request`;
  const toSign = ["AWS4-HMAC-SHA256", amzDate, scope, hex(canonical)].join("\n");
  const signingKey = [date, key.region, key.service, "aws4_request"].reduce<Buffer>(
    (previous, part) => hmac(previous, part),
    Buffer.from(`AWS4${key.secretAccessKey}`),
  );
  const signature = hmac(signingKey, toSign).toString("hex");
  return {
    ...signed,
    authorization: `AWS4-HMAC-SHA256 Credential=${key.accessKeyId}/${scope}, SignedHeaders=${names.join(";")}, Signature=${signature}`,
  };
}

function lowercase(headers: Record<string, string>): Record<string, string> {
  return Object.fromEntries(Object.entries(headers).map(([name, value]) => [name.toLowerCase(), value]));
}

/** RFC 3986 percent-encoding, as SigV4 requires. */
function encode(value: string): string {
  return encodeURIComponent(value).replace(/[!'()*]/g, (c) => `%${c.charCodeAt(0).toString(16).toUpperCase()}`);
}

function compare(a: string, b: string): number {
  return a < b ? -1 : a > b ? 1 : 0;
}

function hex(data: string): string {
  return createHash("sha256").update(data).digest("hex");
}

function hmac(key: Buffer, data: string): Buffer {
  return createHmac("sha256", key).update(data).digest();
}
