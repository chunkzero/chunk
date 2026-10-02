import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";

import { Code } from "@connectrpc/connect";

import { DeploymentService, ReleaseState } from "../src/gen/chunk/management/v1/deployments_pb.ts";
import { signV4 } from "../src/logstore/sts.ts";
import { s3ReleaseStore } from "../src/releases/s3-store.ts";
import { releaseKey, type UploadTarget } from "../src/releases/store.ts";
import { releaseArchive } from "./fixtures.ts";
import { codeOf, createEnvironment, databaseUrl, type Harness, startHarness } from "./harness.ts";

/** A MinIO server to run against; see logstore-minio.test.ts. */
const minioUrl = process.env.TEST_MINIO_URL;
const root = {
  accessKeyId: process.env.TEST_MINIO_ACCESS_KEY_ID ?? "chunkroot",
  secretAccessKey: process.env.TEST_MINIO_SECRET_ACCESS_KEY ?? "chunkrootsecret",
};

describe.skipIf(!databaseUrl || !minioUrl)("S3 release store against MinIO", () => {
  const bucket = `chunk-releases-${crypto.randomUUID().slice(0, 8)}`;
  const endpoint = new URL(minioUrl ?? "http://127.0.0.1");
  // Another name for the same server, so upload URLs show which endpoint signed them.
  const publicEndpoint = new URL(endpoint);
  publicEndpoint.hostname = "localhost";
  let h: Harness;

  beforeAll(async () => {
    const url = new URL(`/${bucket}`, endpoint);
    const emptySha256 = createHash("sha256").update("").digest("hex");
    const headers = signV4(
      { method: "PUT", url, headers: { "x-amz-content-sha256": emptySha256 } },
      { ...root, region: "us-east-1", service: "s3" },
    );
    expect((await fetch(url, { method: "PUT", headers })).status).toBe(200);
    const releases = s3ReleaseStore({
      endpoint: endpoint.origin,
      publicEndpoint: publicEndpoint.origin,
      region: "us-east-1",
      bucket,
      prefix: "releases/",
      ...root,
    });
    h = await startHarness({ releases });
  });
  afterAll(() => h.close());

  const put = async (target: UploadTarget | undefined, body: Uint8Array) =>
    (await fetch(target?.url ?? "", { method: target?.method ?? "", headers: target?.headers ?? {}, body })).status;

  test("uploads go through presigned URLs, and only verified archives are stored and downloaded", async () => {
    const { projectId } = await createEnvironment(h);
    const deployments = h.client(DeploymentService);
    const archive = releaseArchive("s1");
    const key = releaseKey(projectId, "s1", archive.sha256);
    const { upload } = await deployments.uploadRelease({
      projectId,
      releaseId: "s1",
      archiveSha256: archive.sha256,
      archiveSizeBytes: archive.sizeBytes,
    });
    expect(upload?.url).toStartWith(`${publicEndpoint.origin}/${bucket}/releases/uploads/`);
    const complete = () => deployments.completeReleaseUpload({ projectId, releaseId: "s1" });

    const tampered = new Uint8Array(archive.bytes);
    tampered[100] = (tampered[100] ?? 0) ^ 0xff;
    expect(await put(upload, tampered)).toBe(200);
    expect(await codeOf(complete())).toBe(Code.FailedPrecondition);
    expect(await h.deps.releases.exists(key)).toBe(false);

    expect(await put(upload, archive.bytes)).toBe(200);
    expect((await complete()).release?.state).toBe(ReleaseState.READY);

    // The upload URL still works, but what it uploads no longer reaches the stored archive.
    expect(await put(upload, tampered)).toBe(200);
    expect((await complete()).release?.state).toBe(ReleaseState.READY);
    const url = await h.deps.releases.downloadUrl(key, new Date(Date.now() + 60_000));
    expect(url).toStartWith(`${endpoint.origin}/${bucket}/releases/archives/`);
    const downloaded = new Uint8Array(await (await fetch(url)).arrayBuffer());
    expect(createHash("sha256").update(downloaded).digest("hex")).toBe(archive.sha256);
  });

  test("an upload replaced after it was verified is not stored", async () => {
    const releases = h.deps.releases;
    const archive = releaseArchive("s2");
    const expected = { sha256: archive.sha256, sizeBytes: archive.sizeBytes };
    const key = releaseKey("p", "s2", archive.sha256);
    const target = await releases.uploadTarget(key, expected, new Date(Date.now() + 60_000));
    expect(await put(target, archive.bytes)).toBe(200);
    const completing = releases.complete(key, expected, async (stream) => {
      await new Response(stream).arrayBuffer();
      expect(await put(target, new Uint8Array(archive.bytes.byteLength))).toBe(200);
    });
    await expect(completing).rejects.toThrow("release store copy failed");
    expect(await releases.exists(key)).toBe(false);
  });
});
