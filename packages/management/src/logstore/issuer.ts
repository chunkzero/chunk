import type { LogStore } from "../config.ts";
import { deletePrefix } from "./s3.ts";
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
  /** Credentials that only read and list the environment's log prefix, for forks restoring from it and for listings. */
  readGrant(environmentId: string): Promise<LogStoreGrant>;
  /**
   * Removes the environment's log objects, stopping once `signal` aborts. A deleted environment's are removed once its
   * machines are gone and before the environment is. The reconciler gives up on a call that outlasts its provider call
   * timeout, and retries a failed or abandoned one on a later pass.
   */
  deleteEnvironment(environmentId: string, signal: AbortSignal): Promise<void>;
}

/** Credentials are replaced once less than a third of their lifetime, and at most this long, is left. */
const maxRefreshBeforeMs = 15 * 60 * 1000;
/** How long an AssumeRole request, its response body included, may take before it fails and is no longer cached. */
const stsTimeoutMs = 30_000;

export function logStoreIssuer(store: LogStore, fetchImpl: typeof fetch = fetch): LogStoreIssuer {
  return store.sharedCredentials ? sharedIssuer(store) : stsIssuer(store, fetchImpl);
}

/**
 * Every environment gets the operator's credentials, read grants included; only for installs that trust all
 * environments' code.
 */
function sharedIssuer(store: LogStore): LogStoreIssuer {
  const grant = (environmentId: string): LogStoreGrant => ({
    ...location(store, environmentId),
    accessKeyId: store.accessKeyId,
    secretAccessKey: store.secretAccessKey,
    sessionToken: "",
    expireTime: undefined,
  });
  return {
    grant: async (environmentId) => grant(environmentId),
    readGrant: async (environmentId) => grant(environmentId),
    deleteEnvironment: (environmentId, signal) => deletePrefix(grant(environmentId), signal),
  };
}

type Access = "write" | "read";

/**
 * Temporary credentials from STS AssumeRole, limited by an inline session policy to the environment's prefix, and for
 * read grants to reading and listing it. Works with AWS S3 and MinIO. An environment's objects are deleted with its
 * own credentials.
 */
function stsIssuer(store: LogStore, fetchImpl: typeof fetch): LogStoreIssuer {
  const cache = new Map<string, { grant: Promise<LogStoreGrant>; refreshAt: number }>();
  const assume = async (environmentId: string, access: Access) => {
    const issuedAt = Date.now();
    const target = location(store, environmentId);
    const body = new URLSearchParams({
      Action: "AssumeRole",
      Version: "2011-06-15",
      RoleArn: store.roleArn,
      RoleSessionName: `chunk-${access === "read" ? "read-" : ""}${environmentId}`.slice(0, 64),
      DurationSeconds: String(store.credentialSeconds),
      Policy: JSON.stringify(prefixPolicy(store.bucket, target.prefix, access === "read" ? readActions : writeActions)),
    }).toString();
    const url = new URL(store.stsEndpoint);
    const headers = signV4(
      { method: "POST", url, headers: { "content-type": "application/x-www-form-urlencoded" }, body },
      { accessKeyId: store.accessKeyId, secretAccessKey: store.secretAccessKey, region: store.region, service: "sts" },
    );
    const response = await fetchImpl(url, { method: "POST", headers, body, signal: AbortSignal.timeout(stsTimeoutMs) });
    const xml = await response.text();
    // Only the error's code: a response can echo the request.
    const code = /<Code>(\w+)<\/Code>/.exec(xml)?.[1];
    if (!response.ok) throw new Error(`STS AssumeRole failed with HTTP ${response.status}${code ? `: ${code}` : ""}`);
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
  const grant = (environmentId: string, access: Access) => {
    const key = `${access}/${environmentId}`;
    const cached = cache.get(key);
    if (cached && Date.now() < cached.refreshAt) return cached.grant;
    // Cached before it settles, so concurrent grants share one AssumeRole; it is refreshed once it has settled.
    const issued = assume(environmentId, access);
    const entry = { grant: issued.then(({ grant }) => grant), refreshAt: Number.POSITIVE_INFINITY };
    cache.set(key, entry);
    issued.then(
      ({ refreshAt }) => {
        entry.refreshAt = refreshAt;
      },
      () => {
        if (cache.get(key) === entry) cache.delete(key);
      },
    );
    return entry.grant;
  };
  return {
    grant: (environmentId) => grant(environmentId, "write"),
    readGrant: (environmentId) => grant(environmentId, "read"),
    async deleteEnvironment(environmentId, signal) {
      await deletePrefix(await grant(environmentId, "write"), signal);
      cache.delete(`write/${environmentId}`);
      cache.delete(`read/${environmentId}`);
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

const writeActions = ["s3:GetObject", "s3:PutObject", "s3:DeleteObject", "s3:AbortMultipartUpload"];
const readActions = ["s3:GetObject"];

/** `actions` on objects below `prefix` and listing them, and nothing else: by default reading and writing them. */
export function prefixPolicy(bucket: string, prefix: string, actions = writeActions) {
  return {
    Version: "2012-10-17",
    Statement: [
      {
        Effect: "Allow",
        Action: actions,
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
