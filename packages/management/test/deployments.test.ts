import { afterAll, beforeAll, describe, expect, spyOn, test } from "bun:test";
import { createHash } from "node:crypto";
import * as fs from "node:fs/promises";

import { Code } from "@connectrpc/connect";

import { activateDeployment, failDeployment } from "../src/deployments/store.ts";
import { desiredDeployment } from "../src/environments/desired.ts";
import { DeploymentState } from "../src/gen/chunk/management/v1/common_pb.ts";
import { DeploymentService, DeploymentTrigger, ReleaseState } from "../src/gen/chunk/management/v1/deployments_pb.ts";
import { ProjectService } from "../src/gen/chunk/management/v1/projects_pb.ts";
import { releaseKey } from "../src/releases/store.ts";
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

  type Archive = ReturnType<typeof releaseArchive>;

  async function declare(id: string, archive: Archive) {
    const started = await h.client(DeploymentService).uploadRelease({
      projectId,
      releaseId: id,
      archiveSha256: archive.sha256,
      archiveSizeBytes: archive.sizeBytes,
    });
    return started.upload?.url ?? "";
  }

  async function put(url: string, archive: Archive) {
    return (await fetch(url, { method: "PUT", body: archive.bytes })).status;
  }

  async function upload(id: string, archive = releaseArchive(id)) {
    expect(await put(await declare(id, archive), archive)).toBe(204);
    return h.client(DeploymentService).completeReleaseUpload({ projectId, releaseId: id });
  }

  test("uploads verify the archive before a release becomes READY", async () => {
    const deployments = h.client(DeploymentService);
    const archive = releaseArchive("r0");
    const url = await declare("r0", archive);
    const tampered = new Uint8Array(archive.bytes);
    tampered[100] = (tampered[100] ?? 0) ^ 0xff;
    expect((await fetch(url, { method: "PUT", body: tampered })).status).toBe(400);
    expect(await put(url.replace("r0", "r9"), archive)).toBe(403);
    expect(await codeOf(deployments.completeReleaseUpload({ projectId, releaseId: "r0" }))).toBe(
      Code.FailedPrecondition,
    );
    expect(await codeOf(upload("r0", releaseArchive("other")))).toBe(Code.FailedPrecondition);

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

  test("an incomplete short write fails without publishing and a correct upload can complete", async () => {
    const id = "short-write";
    const archive = releaseArchive(id);
    const url = await declare(id, archive);
    const open = fs.open;
    let written = 0;
    const opening = spyOn(fs, "open").mockImplementation(async (...args) => {
      const file = await open(...args);
      if (String(args[0]).endsWith(".partial")) {
        // Write only part of the chunk, then fail any attempt to finish it.
        file.write = new Proxy(file.write, {
          async apply(write, receiver, args) {
            if (written > 0) throw Object.assign(new Error("disk full"), { code: "ENOSPC" });
            const chunk = args[0] as Uint8Array;
            const result = await Reflect.apply(write, receiver, [chunk, 0, Math.floor(chunk.byteLength / 2)]);
            written = result.bytesWritten;
            return result;
          },
        });
      }
      return file;
    });
    let accepted: boolean;
    try {
      const body = new ReadableStream<Uint8Array>({
        start(controller) {
          controller.enqueue(archive.bytes);
          controller.close();
        },
      });
      accepted = await h.fetch(new Request(url, { method: "PUT", body })).then(
        (response) => response.ok,
        () => false,
      );
    } finally {
      opening.mockRestore();
    }
    expect(written).toBeGreaterThan(0);
    expect(written).toBeLessThan(archive.bytes.byteLength);
    expect(accepted).toBe(false);
    expect(await h.releases.read(releaseKey(projectId, id, archive.sha256))).toBeUndefined();
    expect(await put(url, archive)).toBe(204);
    const completed = await h.client(DeploymentService).completeReleaseUpload({ projectId, releaseId: id });
    expect(completed.release?.state).toBe(ReleaseState.READY);
  });

  test("an upload URL from an earlier declaration never replaces the READY archive", async () => {
    const first = releaseArchive("r5");
    const second = releaseArchive("r5");
    const stale = await declare("r5", first);
    expect((await upload("r5", second)).release?.archiveSha256).toBe(second.sha256);
    expect(await put(stale, first)).toBe(204);

    const stored = await h.releases.read(releaseKey(projectId, "r5", second.sha256));
    const bytes = new Uint8Array(await new Response(stored).arrayBuffer());
    expect(createHash("sha256").update(bytes).digest("hex")).toBe(second.sha256);
  });

  test("completion finalizes only the declaration it verified", async () => {
    const deployments = h.client(DeploymentService);
    const archive = releaseArchive("r6");
    expect(await put(await declare("r6", archive), archive)).toBe(204);
    h.beforeRead = async () => {
      h.beforeRead = undefined;
      await declare("r6", { ...archive, sizeBytes: archive.sizeBytes + 1n });
    };
    const complete = () => deployments.completeReleaseUpload({ projectId, releaseId: "r6" });
    expect(await codeOf(complete())).toBe(Code.Aborted);
    expect(await codeOf(complete())).toBe(Code.FailedPrecondition);
    await declare("r6", archive);
    expect((await complete()).release?.archiveSizeBytes).toBe(archive.sizeBytes);
  });

  test("a superseded deployment the environment activated replaces an older active one", async () => {
    const deployments = h.client(DeploymentService);
    const projects = h.client(ProjectService);
    const environmentId =
      (await projects.createEnvironment({ requestId: crypto.randomUUID(), projectId, name: "canary" })).environment
        ?.id ?? "";
    await upload("r7");
    const deploy = async () =>
      (await deployments.deploy({ requestId: crypto.randomUUID(), environmentId, releaseId: "r7" })).deployment?.id ??
      "";
    const states = (...ids: string[]) =>
      Promise.all(
        ids.map(async (deploymentId) => (await deployments.getDeployment({ deploymentId })).deployment?.state),
      );

    const a = await deploy();
    await activateDeployment(h.db, a);
    const b = await deploy();
    const c = await deploy();
    await activateDeployment(h.db, b);
    await activateDeployment(h.db, a);
    expect(await states(a, b, c)).toEqual([
      DeploymentState.SUPERSEDED,
      DeploymentState.ACTIVE,
      DeploymentState.PENDING,
    ]);
    expect((await projects.getEnvironment({ environmentId })).environment?.activeDeploymentId).toBe(b);
    expect((await desiredDeployment(h.db, environmentId))?.id).toBe(c);

    await failDeployment(h.db, c, "rejected");
    expect((await desiredDeployment(h.db, environmentId))?.id).toBe(b);
  });

  test("a release whose Java version has no JVM image is refused at deploy", async () => {
    await upload("r-java", releaseArchive("r-java", undefined, { manifest: (m) => ({ ...m, java_version: "25" }) }));
    const deploy = { requestId: crypto.randomUUID(), environmentId: staging, releaseId: "r-java" };
    expect(await codeOf(h.client(DeploymentService).deploy(deploy))).toBe(Code.FailedPrecondition);
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
    await activateDeployment(h.db, r2);
    const r3 = (await deploy(staging, "r3")).deployment?.id ?? "";
    await activateDeployment(h.db, r3);
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
