import { afterAll, beforeAll, describe, expect, test } from "bun:test";

import { Code } from "@connectrpc/connect";

import { activateDeployment } from "../src/deployments/store.ts";
import { desiredState } from "../src/environments/desired.ts";
import { claimLease } from "../src/environments/store.ts";
import { DeploymentService, DeploymentTrigger } from "../src/gen/chunk/management/v1/deployments_pb.ts";
import { ProjectService } from "../src/gen/chunk/management/v1/projects_pb.ts";
import { SecretService } from "../src/gen/chunk/management/v1/secrets_pb.ts";
import { logStoreIssuer } from "../src/logstore/issuer.ts";
import { codeOf, createEnvironment, databaseUrl, deployRelease, type Harness, startHarness } from "./harness.ts";

/** Object keys and their modification times, listed by a fake S3 endpoint two keys per page. */
const objects = new Map<string, Date>();

function fakeS3() {
  return Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    fetch(request) {
      const url = new URL(request.url);
      const prefix = url.searchParams.get("prefix") ?? "";
      const keys = [...objects.keys()].filter((key) => key.startsWith(prefix)).sort();
      const start = Number(url.searchParams.get("continuation-token") ?? 0);
      const page = keys.slice(start, start + 2);
      const truncated = start + 2 < keys.length;
      const contents = page.map(
        (key) =>
          `<Contents><Key>${key}</Key><LastModified>${objects.get(key)?.toISOString()}</LastModified>` +
          `<Size>1</Size></Contents>`,
      );
      return new Response(
        `<?xml version="1.0" encoding="UTF-8"?><ListBucketResult><Name>logs</Name><Prefix>${prefix}</Prefix>` +
          `<KeyCount>${page.length}</KeyCount><MaxKeys>2</MaxKeys><IsTruncated>${truncated}</IsTruncated>` +
          (truncated ? `<NextContinuationToken>${start + 2}</NextContinuationToken>` : "") +
          `${contents.join("")}</ListBucketResult>`,
        { headers: { "content-type": "application/xml" } },
      );
    },
  });
}

/** Stores the objects core writes for a snapshot of `environmentId` at `epoch` and `sequence`, and a segment after it. */
function snapshot(environmentId: string, epoch: number, sequence: number, time = new Date()) {
  const pad = (value: number) => String(value).padStart(20, "0");
  const directory = `environments/${environmentId}/epochs/${pad(epoch)}`;
  objects.set(`${directory}/claim`, time);
  objects.set(`${directory}/snapshots/${pad(sequence)}.db`, time);
  objects.set(`${directory}/segments/${pad(sequence + 1)}-${pad(sequence + 2)}.log`, time);
}

describe.skipIf(!databaseUrl)("environment forks", () => {
  let h: Harness;
  let s3: ReturnType<typeof fakeS3>;
  beforeAll(async () => {
    s3 = fakeS3();
    const logStore = logStoreIssuer({
      endpoint: s3.url.origin,
      region: "us-east-1",
      bucket: "logs",
      prefix: "environments/",
      accessKeyId: "operator",
      secretAccessKey: "secret",
      sharedCredentials: true,
      stsEndpoint: "",
      roleArn: "",
      credentialSeconds: 3600,
    });
    h = await startHarness({ logStore });
  });
  afterAll(async () => {
    await h.close();
    await s3.stop(true);
  });

  test("lists the snapshots stored in an environment's log, newest first, a page at a time", async () => {
    const { environmentId } = await createEnvironment(h);
    const first = new Date("2026-09-01T00:00:00Z");
    snapshot(environmentId, 1, 5, first);
    snapshot(environmentId, 2, 3);
    snapshot(environmentId, 2, 9);
    const projects = h.client(ProjectService);

    const page = await projects.listSnapshots({ environmentId, pageSize: 2 });
    expect(page.snapshots.map((snapshot) => snapshot.id)).toEqual(["2-9", "2-3"]);
    expect(page.snapshots[0]).toMatchObject({ environmentId, epoch: 2n, logSequence: 9n });
    const next = await projects.listSnapshots({ environmentId, pageSize: 2, pageToken: page.nextPageToken });
    expect(next.snapshots.map((snapshot) => snapshot.id)).toEqual(["1-5"]);
    expect(next.snapshots[0]?.createTime?.seconds).toBe(BigInt(first.getTime() / 1000));
    expect(next.nextPageToken).toBe("");
  });

  test("a fork deploys its source's active release, copies secrets on request, and restores until its core attaches", async () => {
    const { projectId, environmentId: sourceId } = await createEnvironment(h);
    const projects = h.client(ProjectService);
    const fork = (name: string, snapshotId = "") =>
      projects.forkEnvironment({
        requestId: crypto.randomUUID(),
        sourceEnvironmentId: sourceId,
        name,
        snapshotId,
        copySecrets: true,
      });
    expect(await codeOf(fork("preview"))).toBe(Code.FailedPrecondition);
    snapshot(sourceId, 1, 4);
    expect(await codeOf(fork("preview"))).toBe(Code.FailedPrecondition);
    await activateDeployment(h.sql, await deployRelease(h, projectId, sourceId, "rel_fork"));
    await h.client(SecretService).setSecret({
      requestId: crypto.randomUUID(),
      environmentId: sourceId,
      name: "API_KEY",
      value: new TextEncoder().encode("hunter2"),
    });
    expect(await codeOf(fork("preview", "1-5"))).toBe(Code.NotFound);

    const forked = (await fork("preview", "1-4")).environment;
    const forkId = forked?.id ?? "";
    expect(forked).toMatchObject({ projectId, forkedFromEnvironmentId: sourceId, forkedFromSnapshotId: "1-4" });
    const [deployment] = (await h.client(DeploymentService).listDeployments({ environmentId: forkId })).deployments;
    expect(deployment).toMatchObject({ releaseId: "rel_fork", trigger: DeploymentTrigger.FORK });

    const { message } = await desiredState(h.deps, forkId);
    expect(message.deploymentId).toBe(deployment?.id ?? "");
    expect(message.secrets.map(({ name, value }) => [name, new TextDecoder().decode(value)])).toEqual([
      ["API_KEY", "hunter2"],
    ]);
    expect(message.logStore?.prefix).toBe(`environments/${forkId}/`);
    expect(message.restore).toMatchObject({ snapshotId: "1-4", sourceEnvironmentId: sourceId });
    expect(message.restore?.source?.prefix).toBe(`environments/${sourceId}/`);

    await claimLease(h.sql, forkId, crypto.randomUUID(), 1n);
    expect((await desiredState(h.deps, forkId)).message.restore).toBeUndefined();
  });
});
