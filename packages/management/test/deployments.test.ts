import { afterAll, beforeAll, describe, expect, test } from "bun:test";

import { Code } from "@connectrpc/connect";

import { activateDeployment } from "../src/deployments/store.ts";
import { DeploymentState } from "../src/gen/chunk/management/v1/common_pb.ts";
import { DeploymentService, DeploymentTrigger, ReleaseState } from "../src/gen/chunk/management/v1/deployments_pb.ts";
import { ProjectService } from "../src/gen/chunk/management/v1/projects_pb.ts";
import { releaseArchive } from "./fixtures.ts";
import { codeOf, databaseUrl, type Harness, startHarness } from "./harness.ts";

describe.skipIf(!databaseUrl)("DeploymentService", () => {
  let h: Harness;
  let projectId: string;
  let staging: string;
  let production: string;
  beforeAll(async () => {
    h = await startHarness();
    const projects = h.client(ProjectService);
    projectId = (await projects.createProject({ requestId: crypto.randomUUID(), name: "game" })).project?.id ?? "";
    const environment = async (name: string) =>
      (await projects.createEnvironment({ requestId: crypto.randomUUID(), projectId, name })).environment?.id ?? "";
    staging = await environment("staging");
    production = await environment("production");
  });
  afterAll(() => h.close());

  async function upload(id: string, contents = releaseArchive({ id, apps: [{ id: "lobby", sessions: ["default"] }] })) {
    const deployments = h.client(DeploymentService);
    const started = await deployments.uploadRelease({
      projectId,
      releaseId: id,
      archiveSha256: contents.sha256,
      archiveSizeBytes: contents.sizeBytes,
    });
    const target = started.upload;
    if (!target) throw new Error("expected an upload target");
    const response = await fetch(target.url, { method: target.method, headers: target.headers, body: contents.bytes });
    expect(response.status).toBe(204);
    return deployments.completeReleaseUpload({ projectId, releaseId: id });
  }

  test("uploads verify the archive before a release becomes READY", async () => {
    const deployments = h.client(DeploymentService);
    const archive = releaseArchive({ id: "r0", apps: [] });
    const started = await deployments.uploadRelease({
      projectId,
      releaseId: "r0",
      archiveSha256: archive.sha256,
      archiveSizeBytes: archive.sizeBytes,
    });
    expect(started.release?.state).toBe(ReleaseState.UPLOADING);
    const url = started.upload?.url ?? "";
    const tampered = new Uint8Array(archive.bytes);
    tampered[100] = (tampered[100] ?? 0) ^ 0xff;
    expect((await fetch(url, { method: "PUT", body: tampered })).status).toBe(400);
    expect((await fetch(url.replace("r0", "r9"), { method: "PUT", body: archive.bytes })).status).toBe(403);
    expect(await codeOf(deployments.completeReleaseUpload({ projectId, releaseId: "r0" }))).toBe(
      Code.FailedPrecondition,
    );

    const mislabeled = releaseArchive({ id: "other", apps: [] });
    expect(await codeOf(upload("r0", mislabeled))).toBe(Code.FailedPrecondition);

    const ready = await upload("r1");
    expect(ready.release?.state).toBe(ReleaseState.READY);
    const again = await deployments.uploadRelease({
      projectId,
      releaseId: "r1",
      archiveSha256: ready.release?.archiveSha256 ?? "",
      archiveSizeBytes: ready.release?.archiveSizeBytes ?? 0n,
    });
    expect(again.upload).toBeUndefined();
  });

  test("deploy, promote and rollback move releases between environments", async () => {
    const deployments = h.client(DeploymentService);
    await upload("r2");
    await upload("r3");

    const deploy = (environmentId: string, releaseId: string) =>
      deployments.deploy({ requestId: crypto.randomUUID(), environmentId, releaseId });
    const first = (await deploy(staging, "r2")).deployment;
    const second = (await deploy(staging, "r3")).deployment;
    expect(second?.state).toBe(DeploymentState.PENDING);
    expect((await deployments.getDeployment({ deploymentId: first?.id ?? "" })).deployment?.state).toBe(
      DeploymentState.SUPERSEDED,
    );
    expect(await codeOf(deploy(staging, "missing"))).toBe(Code.NotFound);

    // The environment reports serving r2, then r3; part 2's ReportStatus does this.
    const r2 = (await deploy(staging, "r2")).deployment?.id ?? "";
    await activateDeployment(h.sql, r2);
    const r3 = (await deploy(staging, "r3")).deployment?.id ?? "";
    await activateDeployment(h.sql, r3);
    expect((await deployments.getDeployment({ deploymentId: r2 })).deployment?.state).toBe(DeploymentState.SUPERSEDED);
    expect((await deployments.listApps({ environmentId: staging })).apps).toEqual([
      expect.objectContaining({ id: "lobby", sessions: ["default"] }),
    ]);

    const promoted = await deployments.promote({
      requestId: crypto.randomUUID(),
      sourceEnvironmentId: staging,
      targetEnvironmentId: production,
    });
    expect(promoted.deployment?.releaseId).toBe("r3");
    expect(promoted.deployment?.trigger).toBe(DeploymentTrigger.PROMOTE);

    const rollback = { requestId: crypto.randomUUID(), environmentId: staging, deploymentId: "" };
    const rolledBack = await deployments.rollback(rollback);
    expect(rolledBack.deployment?.releaseId).toBe("r2");
    expect((await deployments.rollback(rollback)).deployment?.id).toBe(rolledBack.deployment?.id ?? "");
    expect(await codeOf(deployments.rollback({ ...rollback, deploymentId: first?.id ?? "" }))).toBe(Code.AlreadyExists);

    const newestFirst = await deployments.listDeployments({ environmentId: staging, pageSize: 2 });
    expect(newestFirst.deployments[0]?.id).toBe(rolledBack.deployment?.id ?? "");
    expect(newestFirst.nextPageToken).not.toBe("");
  });
});
