import { createHash, createHmac } from "node:crypto";

/** How long an AssumeRole request, its response body included, may take. */
const stsTimeoutMs = 30_000;

export interface AssumedCredentials {
  accessKeyId: string;
  secretAccessKey: string;
  sessionToken: string;
  expiration: Date;
}

/** Temporary credentials from STS `AssumeRole`, requested with the operator's and limited by an inline `policy`. */
export async function assumeRole(
  sts: { endpoint: string; region: string; accessKeyId: string; secretAccessKey: string },
  role: { roleArn: string; sessionName: string; durationSeconds: number; policy: unknown },
  fetchImpl: typeof fetch,
): Promise<AssumedCredentials> {
  const body = new URLSearchParams({
    Action: "AssumeRole",
    Version: "2011-06-15",
    RoleArn: role.roleArn,
    RoleSessionName: role.sessionName,
    DurationSeconds: String(role.durationSeconds),
    Policy: JSON.stringify(role.policy),
  }).toString();
  const url = new URL(sts.endpoint);
  const headers = signV4(
    { method: "POST", url, headers: { "content-type": "application/x-www-form-urlencoded" }, body },
    { ...sts, service: "sts" },
  );
  const response = await fetchImpl(url, { method: "POST", headers, body, signal: AbortSignal.timeout(stsTimeoutMs) });
  const xml = await response.text();
  // Only the error's code: a response can echo the request.
  const code = /<Code>(\w+)<\/Code>/.exec(xml)?.[1];
  if (!response.ok) throw new Error(`STS AssumeRole failed with HTTP ${response.status}${code ? `: ${code}` : ""}`);
  const expiration = new Date(element(xml, "Expiration"));
  if (Number.isNaN(expiration.getTime())) throw new Error("STS AssumeRole returned no expiration");
  return {
    accessKeyId: element(xml, "AccessKeyId"),
    secretAccessKey: element(xml, "SecretAccessKey"),
    sessionToken: element(xml, "SessionToken"),
    expiration,
  };
}

/** The unescaped text of the reply's first `<name>` element. */
function element(xml: string, name: string): string {
  const value = new RegExp(`<${name}>([^<]*)</${name}>`)
    .exec(xml)?.[1]
    ?.replaceAll("&lt;", "<")
    .replaceAll("&gt;", ">")
    .replaceAll("&quot;", '"')
    .replaceAll("&apos;", "'")
    .replaceAll("&amp;", "&");
  if (value === undefined) throw new Error(`STS AssumeRole returned no ${name}`);
  return value;
}

interface SigningKey {
  accessKeyId: string;
  secretAccessKey: string;
  region: string;
  service: string;
}

/**
 * The headers that sign a request with AWS Signature Version 4: `host`, `x-amz-date` and `authorization`, plus the
 * given ones, which are all signed. Only STS needs it; S3 requests go through `Bun.S3Client`.
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
