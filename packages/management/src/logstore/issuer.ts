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
 * issuer caches and returns fresh credentials well before the old ones expire. A hosted install can plug in its own.
 */
export interface LogStoreIssuer {
  grant(environmentId: string): Promise<LogStoreGrant>;
}

/** Credentials are replaced once less than a third of their lifetime, and at most this long, is left. */
const maxRefreshBeforeMs = 15 * 60 * 1000;

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
  const cache = new Map<string, { grant: Promise<LogStoreGrant>; refreshAt: number }>();
  const assume = async (environmentId: string) => {
    const issuedAt = Date.now();
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
    const grant: LogStoreGrant = {
      ...target,
      accessKeyId: element(xml, "AccessKeyId"),
      secretAccessKey: element(xml, "SecretAccessKey"),
      sessionToken: element(xml, "SessionToken"),
      expireTime: expiration,
    };
    const lifetime = expiration.getTime() - issuedAt;
    return { grant, refreshAt: expiration.getTime() - Math.min(maxRefreshBeforeMs, lifetime / 3) };
  };
  return {
    grant(environmentId) {
      const cached = cache.get(environmentId);
      if (cached && Date.now() < cached.refreshAt) return cached.grant;
      // Cached before it settles, so concurrent grants share one AssumeRole; it is refreshed once it has settled.
      const issued = assume(environmentId);
      const entry = { grant: issued.then(({ grant }) => grant), refreshAt: Number.POSITIVE_INFINITY };
      cache.set(environmentId, entry);
      issued.then(
        ({ refreshAt }) => {
          entry.refreshAt = refreshAt;
        },
        () => {
          if (cache.get(environmentId) === entry) cache.delete(environmentId);
        },
      );
      return entry.grant;
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
