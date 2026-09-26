import type { LogStore } from "../config.ts";
import { signV4 } from "./sigv4.ts";

/** Object-store access for one environment's log prefix. */
export interface LogStoreGrant {
  endpoint: string;
  region: string;
  bucket: string;
  prefix: string;
  accessKeyId: string;
  secretAccessKey: string;
  sessionToken: string;
  /** Unset for credentials that do not expire. */
  expireTime: Date | undefined;
}

/**
 * Hands each environment credentials for its own log prefix. Attach calls it for every message it sends, so an
 * issuer caches and returns fresh credentials well before the old ones expire. Chunk Cloud plugs in its own.
 */
export interface LogStoreIssuer {
  grant(environmentId: string): Promise<LogStoreGrant>;
}

/** Credentials are replaced once less than this much of their lifetime is left. */
const refreshBeforeMs = 15 * 60 * 1000;

export function logStoreIssuer(store: LogStore, fetchImpl: typeof fetch = fetch): LogStoreIssuer {
  return store.sharedCredentials ? sharedIssuer(store) : stsIssuer(store, fetchImpl);
}

/** Every environment gets the operator's credentials; only for installs that trust all environments' code. */
function sharedIssuer(store: LogStore): LogStoreIssuer {
  return {
    async grant(environmentId) {
      return {
        ...location(store, environmentId),
        accessKeyId: store.accessKeyId,
        secretAccessKey: store.secretAccessKey,
        sessionToken: "",
        expireTime: undefined,
      };
    },
  };
}

/**
 * Temporary credentials from STS AssumeRole, limited by an inline session policy to the environment's prefix. Works
 * with AWS S3 and MinIO.
 */
function stsIssuer(store: LogStore, fetchImpl: typeof fetch): LogStoreIssuer {
  const cache = new Map<string, Promise<LogStoreGrant>>();
  const assume = async (environmentId: string): Promise<LogStoreGrant> => {
    const target = location(store, environmentId);
    const body = new URLSearchParams({
      Action: "AssumeRole",
      Version: "2011-06-15",
      RoleArn: store.roleArn,
      RoleSessionName: `chunk-${environmentId}`.slice(0, 64),
      DurationSeconds: String(store.credentialSeconds),
      Policy: JSON.stringify(prefixPolicy(store.bucket, target.prefix)),
    }).toString();
    const url = new URL(store.stsEndpoint);
    const headers = signV4(
      { method: "POST", url, headers: { "content-type": "application/x-www-form-urlencoded" }, body },
      { accessKeyId: store.accessKeyId, secretAccessKey: store.secretAccessKey, region: store.region, service: "sts" },
    );
    const response = await fetchImpl(url, { method: "POST", headers, body });
    const xml = await response.text();
    if (!response.ok) throw new Error(`STS AssumeRole failed with HTTP ${response.status}: ${xml.slice(0, 500)}`);
    const expiration = new Date(element(xml, "Expiration"));
    if (Number.isNaN(expiration.getTime())) throw new Error("STS AssumeRole returned no expiration");
    return {
      ...target,
      accessKeyId: element(xml, "AccessKeyId"),
      secretAccessKey: element(xml, "SecretAccessKey"),
      sessionToken: element(xml, "SessionToken"),
      expireTime: expiration,
    };
  };
  return {
    async grant(environmentId) {
      const cached = cache.get(environmentId);
      if (cached) {
        const grant = await cached.catch(() => undefined);
        if (grant && (grant.expireTime?.getTime() ?? 0) - Date.now() > refreshBeforeMs) return grant;
      }
      const issued = assume(environmentId);
      cache.set(environmentId, issued);
      issued.catch(() => cache.delete(environmentId));
      return issued;
    },
  };
}

function location(store: LogStore, environmentId: string) {
  return {
    endpoint: store.endpoint,
    region: store.region,
    bucket: store.bucket,
    prefix: `${store.prefix}${environmentId}/`,
  };
}

/** Reading, writing and listing objects below `prefix`, and nothing else. */
export function prefixPolicy(bucket: string, prefix: string) {
  return {
    Version: "2012-10-17",
    Statement: [
      {
        Effect: "Allow",
        Action: ["s3:GetObject", "s3:PutObject", "s3:DeleteObject", "s3:AbortMultipartUpload"],
        Resource: [`arn:aws:s3:::${bucket}/${prefix}*`],
      },
      {
        Effect: "Allow",
        Action: ["s3:ListBucket"],
        Resource: [`arn:aws:s3:::${bucket}`],
        Condition: { StringLike: { "s3:prefix": [`${prefix}*`] } },
      },
    ],
  };
}

function element(xml: string, name: string): string {
  const value = new RegExp(`<${name}>([^<]*)</${name}>`).exec(xml)?.[1];
  if (value === undefined) throw new Error(`STS AssumeRole returned no ${name}`);
  return value
    .replaceAll("&lt;", "<")
    .replaceAll("&gt;", ">")
    .replaceAll("&quot;", '"')
    .replaceAll("&apos;", "'")
    .replaceAll("&amp;", "&");
}
