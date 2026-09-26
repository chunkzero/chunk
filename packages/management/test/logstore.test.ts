import { afterAll, beforeAll, describe, expect, setSystemTime, test } from "bun:test";

import { loadConfig } from "../src/config.ts";
import type { LogStore } from "../src/config.ts";
import { desiredState } from "../src/environments/desired.ts";
import { logStoreIssuer } from "../src/logstore/issuer.ts";
import { signV4 } from "../src/logstore/sigv4.ts";
import { createEnvironment, databaseUrl, type Harness, startHarness } from "./harness.ts";

test("signV4 matches AWS's documented example", () => {
  const headers = signV4(
    {
      method: "GET",
      url: new URL("https://iam.amazonaws.com/?Action=ListUsers&Version=2010-05-08"),
      headers: { "Content-Type": "application/x-www-form-urlencoded; charset=utf-8" },
    },
    {
      accessKeyId: "AKIDEXAMPLE",
      secretAccessKey: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
      region: "us-east-1",
      service: "iam",
    },
    new Date("2015-08-30T12:36:00Z"),
  );
  expect(headers.authorization).toBe(
    "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, " +
      "SignedHeaders=content-type;host;x-amz-date, " +
      "Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7",
  );
});

test("shared log store credentials need an explicit opt-in", () => {
  const env = {
    DATABASE_URL: "postgres://localhost/chunk",
    CHUNK_SECRET_KEY: Buffer.alloc(32).toString("base64"),
    CHUNK_LOG_STORE_BUCKET: "logs",
    CHUNK_LOG_STORE_ENDPOINT: "http://127.0.0.1:9000",
    CHUNK_LOG_STORE_ACCESS_KEY_ID: "operator",
    CHUNK_LOG_STORE_SECRET_ACCESS_KEY: "secret",
  };
  expect(() => loadConfig(env)).toThrow("CHUNK_LOG_STORE_ROLE_ARN is required");
  expect(loadConfig({ ...env, CHUNK_LOG_STORE_SHARED_CREDENTIALS: "1" }).logStore?.sharedCredentials).toBe(true);
});

describe.skipIf(!databaseUrl)("STS log store credentials", () => {
  let h: Harness;
  let sts: ReturnType<typeof Bun.serve>;
  const calls: { authorization: string; form: URLSearchParams }[] = [];
  beforeAll(async () => {
    h = await startHarness();
    sts = Bun.serve({
      hostname: "127.0.0.1",
      port: 0,
      async fetch(request) {
        const form = new URLSearchParams(await request.text());
        calls.push({ authorization: request.headers.get("authorization") ?? "", form });
        const expiration = new Date(Date.now() + Number(form.get("DurationSeconds")) * 1000).toISOString();
        return new Response(
          `<AssumeRoleResponse><AssumeRoleResult><Credentials><AccessKeyId>temp-${calls.length}</AccessKeyId>` +
            `<SecretAccessKey>s&amp;cret</SecretAccessKey><SessionToken>token</SessionToken>` +
            `<Expiration>${expiration}</Expiration></Credentials></AssumeRoleResult></AssumeRoleResponse>`,
        );
      },
    });
  });
  afterAll(async () => {
    await sts.stop(true);
    await h.close();
  });

  test("each environment gets expiring credentials limited to its own prefix, delivered through Attach", async () => {
    const store: LogStore = {
      endpoint: "http://127.0.0.1:9000",
      region: "us-east-1",
      bucket: "logs",
      prefix: "environments/",
      accessKeyId: "operator",
      secretAccessKey: "secret",
      sharedCredentials: false,
      stsEndpoint: sts.url.origin,
      roleArn: "arn:aws:iam::123456789012:role/chunk-logs",
      credentialSeconds: 3600,
    };
    const { environmentId } = await createEnvironment(h);
    const { message } = await desiredState({ ...h.deps, logStore: logStoreIssuer(store) }, environmentId);
    expect(message.logStore).toMatchObject({
      bucket: "logs",
      prefix: `environments/${environmentId}/`,
      accessKeyId: "temp-1",
      secretAccessKey: "s&cret",
      sessionToken: "token",
    });
    expect(message.logStore?.expireTime?.seconds).toBeGreaterThan(BigInt(Math.floor(Date.now() / 1000)));

    const [call] = calls;
    expect(call?.authorization).toStartWith("AWS4-HMAC-SHA256 Credential=operator/");
    expect(call?.form.get("Action")).toBe("AssumeRole");
    expect(call?.form.get("RoleArn")).toBe(store.roleArn);
    const policy = JSON.parse(call?.form.get("Policy") ?? "{}");
    expect(JSON.stringify(policy)).not.toContain('"arn:aws:s3:::logs/*"');
    expect(policy.Statement[0].Resource).toEqual([`arn:aws:s3:::logs/environments/${environmentId}/*`]);
    expect(policy.Statement[1].Condition.StringLike["s3:prefix"]).toEqual([`environments/${environmentId}/*`]);
  });

  test("credentials are reused until they near expiry", async () => {
    const issuer = logStoreIssuer({
      endpoint: "http://127.0.0.1:9000",
      region: "us-east-1",
      bucket: "logs",
      prefix: "p/",
      accessKeyId: "operator",
      secretAccessKey: "secret",
      sharedCredentials: false,
      stsEndpoint: sts.url.origin,
      roleArn: "arn:aws:iam::123456789012:role/chunk-logs",
      credentialSeconds: 3600,
    });
    const before = calls.length;
    const first = await issuer.grant("env_a");
    expect(await issuer.grant("env_a")).toBe(first);
    await issuer.grant("env_b");
    expect(calls.length).toBe(before + 2);
  });

  test("short-lived credentials are reused for the first two thirds of their lifetime", async () => {
    const issuer = logStoreIssuer({
      endpoint: "http://127.0.0.1:9000",
      region: "us-east-1",
      bucket: "logs",
      prefix: "p/",
      accessKeyId: "operator",
      secretAccessKey: "secret",
      sharedCredentials: false,
      stsEndpoint: sts.url.origin,
      roleArn: "arn:aws:iam::123456789012:role/chunk-logs",
      credentialSeconds: 900,
    });
    const start = Date.now();
    try {
      setSystemTime(start);
      const before = calls.length;
      const first = await issuer.grant("env_short");
      setSystemTime(start + 590_000);
      expect(await issuer.grant("env_short")).toBe(first);
      expect(calls.length).toBe(before + 1);
      setSystemTime(start + 610_000);
      expect(await issuer.grant("env_short")).not.toBe(first);
      expect(calls.length).toBe(before + 2);
    } finally {
      setSystemTime();
    }
  });
});
