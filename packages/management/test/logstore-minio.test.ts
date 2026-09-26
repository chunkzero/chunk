import { beforeAll, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";

import { type LogStoreGrant, logStoreIssuer } from "../src/logstore/issuer.ts";
import { signV4 } from "../src/logstore/sigv4.ts";

/** A MinIO server to run against, for example `cgr.dev/chainguard/minio server /data` with the root user below. */
const minioUrl = process.env.TEST_MINIO_URL;
const root = {
  accessKeyId: process.env.TEST_MINIO_ACCESS_KEY_ID ?? "chunkroot",
  secretAccessKey: process.env.TEST_MINIO_SECRET_ACCESS_KEY ?? "chunkrootsecret",
};

describe.skipIf(!minioUrl)("STS log store credentials against MinIO", () => {
  const bucket = `chunk-test-${crypto.randomUUID().slice(0, 8)}`;
  const url = minioUrl ?? "";

  function s3(method: string, path: string, credentials: typeof root & { sessionToken?: string }, body = "") {
    const target = new URL(path, url);
    const headers = signV4(
      {
        method,
        url: target,
        headers: {
          "x-amz-content-sha256": createHash("sha256").update(body).digest("hex"),
          ...(credentials.sessionToken ? { "x-amz-security-token": credentials.sessionToken } : {}),
        },
        body,
      },
      { ...credentials, region: "us-east-1", service: "s3" },
    );
    return fetch(target, { method, headers, ...(body ? { body } : {}) });
  }

  beforeAll(async () => {
    expect((await s3("PUT", `/${bucket}`, root)).status).toBe(200);
  });

  test("issued credentials write under their own prefix and nowhere else", async () => {
    const issuer = logStoreIssuer({
      endpoint: url,
      region: "us-east-1",
      bucket,
      prefix: "environments/",
      ...root,
      sharedCredentials: false,
      stsEndpoint: url,
      roleArn: "arn:minio:iam:::role/chunk-logs",
      credentialSeconds: 900,
    });
    const grant: LogStoreGrant = await issuer.grant("env_a");
    expect(grant.accessKeyId).not.toBe(root.accessKeyId);
    expect(grant.expireTime?.getTime()).toBeGreaterThan(Date.now());

    const own = await s3("PUT", `/${bucket}/environments/env_a/log/1`, grant, "entry");
    expect(own.status).toBe(200);
    expect(await (await s3("GET", `/${bucket}/environments/env_a/log/1`, grant)).text()).toBe("entry");
    expect((await s3("PUT", `/${bucket}/environments/env_b/log/1`, grant, "entry")).status).toBe(403);
    expect((await s3("PUT", `/${bucket}/other`, grant, "entry")).status).toBe(403);
    expect((await s3("GET", `/${bucket}?list-type=2&prefix=environments/env_a/`, grant)).status).toBe(200);
    expect((await s3("GET", `/${bucket}?list-type=2&prefix=environments/`, grant)).status).toBe(403);
  });
});
