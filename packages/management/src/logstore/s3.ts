import { createHash } from "node:crypto";

import type { LogStoreGrant } from "./issuer.ts";
import { signV4 } from "./sigv4.ts";

/**
 * Deletes every object below the grant's prefix, listing and deleting up to 1000 at a time until none are left. The
 * bucket is addressed by path, as core addresses it.
 */
export async function deletePrefix(grant: LogStoreGrant, fetchImpl: typeof fetch): Promise<void> {
  const bucket = `${grant.endpoint.replace(/\/+$/, "")}/${grant.bucket}`;
  const list = new URL(bucket);
  list.searchParams.set("list-type", "2");
  list.searchParams.set("prefix", grant.prefix);
  for (;;) {
    const listed = await request(grant, fetchImpl, "GET", list);
    const keys = [...listed.matchAll(/<Key>([^<]*)<\/Key>/g)].map(([, key = ""]) => unescapeXml(key));
    if (keys.length === 0) return;
    const body = `<Delete><Quiet>true</Quiet>${keys.map((key) => `<Object><Key>${escapeXml(key)}</Key></Object>`).join("")}</Delete>`;
    const result = await request(grant, fetchImpl, "POST", new URL(`${bucket}?delete`), body, {
      "content-md5": createHash("md5").update(body).digest("base64"),
    });
    const failed = /<Error>[\s\S]*?<\/Error>/.exec(result);
    if (failed) throw new Error(`deleting log objects failed: ${failed[0].slice(0, 500)}`);
  }
}

async function request(
  grant: LogStoreGrant,
  fetchImpl: typeof fetch,
  method: string,
  url: URL,
  body = "",
  headers: Record<string, string> = {},
): Promise<string> {
  const signed = signV4(
    {
      method,
      url,
      headers: {
        ...headers,
        "x-amz-content-sha256": createHash("sha256").update(body).digest("hex"),
        ...(grant.sessionToken ? { "x-amz-security-token": grant.sessionToken } : {}),
      },
      body,
    },
    { accessKeyId: grant.accessKeyId, secretAccessKey: grant.secretAccessKey, region: grant.region, service: "s3" },
  );
  const response = await fetchImpl(url, { method, headers: signed, ...(body ? { body } : {}) });
  const text = await response.text();
  if (!response.ok) throw new Error(`S3 ${method} failed with HTTP ${response.status}: ${text.slice(0, 500)}`);
  return text;
}

function escapeXml(value: string): string {
  return value.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;");
}

export function unescapeXml(value: string): string {
  return value
    .replaceAll("&lt;", "<")
    .replaceAll("&gt;", ">")
    .replaceAll("&quot;", '"')
    .replaceAll("&apos;", "'")
    .replaceAll("&amp;", "&");
}
