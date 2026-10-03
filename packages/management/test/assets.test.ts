import { afterAll, beforeAll, describe, expect, test } from "bun:test";

import { Code } from "@connectrpc/connect";

import { decodeRevision, sha256Hex } from "../src/assets/revision.ts";
import { issueEnvironmentToken } from "../src/auth/tokens.ts";
import { desiredState } from "../src/environments/desired.ts";
import { AssetRevisionState, AssetService } from "../src/gen/chunk/management/v1/assets_pb.ts";
import { DeploymentService } from "../src/gen/chunk/management/v1/deployments_pb.ts";
import { ProjectService } from "../src/gen/chunk/management/v1/projects_pb.ts";
import { blobKey } from "../src/releases/store.ts";
import { assetRevision } from "./fixtures.ts";
import {
  codeOf,
  createEnvironment,
  databaseUrl,
  type Harness,
  startHarness,
  uploadAssets,
  uploadRelease,
} from "./harness.ts";

test("a revision declaring one digest with two sizes is rejected", () => {
  const sha256 = "a".repeat(64);
  const manifest = `{"version":1,"packs":{},"shared":{"a":{"sha256":"${sha256}","size":1},"b":{"sha256":"${sha256}","size":2}},"apps":{}}`;
  expect(() => decodeRevision(new TextEncoder().encode(manifest))).toThrow("different sizes");
});

describe.skipIf(!databaseUrl)("AssetService", () => {
  let h: Harness;
  beforeAll(async () => {
    h = await startHarness();
  });
  afterAll(() => h.close());

  test("completion waits for every blob and checks packs' SHA-1, and blobs upload once per project", async () => {
    const { projectId } = await createEnvironment(h);
    const assets = h.client(AssetService);
    const revision = assetRevision({ packs: { ui: "pack" }, shared: { "a.txt": "a" } });
    const { revision: declared, uploads } = await assets.uploadAssets({ projectId, manifest: revision.manifest });
    expect(declared).toMatchObject({ id: revision.id, state: AssetRevisionState.UPLOADING });
    expect(uploads.map((upload) => upload.sha256).sort()).toEqual([...revision.blobs.keys()].sort());

    const [first, second] = uploads;
    await fetch(first?.upload?.url ?? "", { method: "PUT", body: revision.blobs.get(first?.sha256 ?? "") });
    const complete = () => assets.completeAssetUpload({ projectId, revisionId: revision.id });
    expect(await codeOf(complete())).toBe(Code.FailedPrecondition);
    await fetch(second?.upload?.url ?? "", { method: "PUT", body: revision.blobs.get(second?.sha256 ?? "") });
    expect((await complete()).revision?.state).toBe(AssetRevisionState.READY);

    const next = assetRevision({ packs: { ui: "pack" }, shared: { "a.txt": "a", "b.txt": "b" } });
    const { uploads: nextUploads } = await assets.uploadAssets({ projectId, manifest: next.manifest });
    expect(nextUploads.map((upload) => upload.sha256)).toEqual([sha256Hex(new TextEncoder().encode("b"))]);

    const wrongSha1 = new TextDecoder()
      .decode(assetRevision({ packs: { ui: "pack" } }).manifest)
      .replace(/"sha1":"[0-9a-f]{40}"/, `"sha1":"${"0".repeat(40)}"`);
    const manifest = new TextEncoder().encode(wrongSha1);
    const { revision: lying } = await assets.uploadAssets({ projectId, manifest });
    expect(await codeOf(assets.completeAssetUpload({ projectId, revisionId: lying?.id ?? "" }))).toBe(
      Code.FailedPrecondition,
    );

    const pretty = new TextEncoder().encode(JSON.stringify(JSON.parse(wrongSha1), null, 2));
    expect(await codeOf(assets.uploadAssets({ projectId, manifest: pretty }))).toBe(Code.InvalidArgument);
  });

  test("a blob the store lost is uploaded again, and blocks completing a revision that needs it", async () => {
    const { projectId } = await createEnvironment(h);
    const assets = h.client(AssetService);
    const revision = assetRevision({ shared: { "a.txt": "a" } });
    await uploadAssets(h, projectId, revision);
    const sha256 = [...revision.blobs.keys()][0] ?? "";
    const lostKey = blobKey(projectId, sha256);
    const { exists } = h.deps.releases;
    h.deps.releases.exists = async (key) => key !== lostKey && exists(key);
    try {
      const { uploads } = await assets.uploadAssets({ projectId, manifest: revision.manifest });
      expect(uploads.map((upload) => upload.sha256)).toEqual([sha256]);
      expect(await codeOf(assets.completeAssetUpload({ projectId, revisionId: revision.id }))).toBe(
        Code.FailedPrecondition,
      );
    } finally {
      h.deps.releases.exists = exists;
    }
  });

  test("the head moves only from the head the caller expects", async () => {
    const { projectId } = await createEnvironment(h);
    const assets = h.client(AssetService);
    const a = await uploadAssets(h, projectId, assetRevision({ shared: { "a.txt": "a" } }));
    const b = await uploadAssets(h, projectId, assetRevision({ shared: { "b.txt": "b" } }));
    expect((await assets.getAssetRevision({ projectId })).revision).toBeUndefined();

    expect((await assets.setAssetHead({ projectId, revisionId: a, expectedHeadId: "" })).previousHeadId).toBe("");
    expect(await codeOf(assets.setAssetHead({ projectId, revisionId: b, expectedHeadId: "" }))).toBe(Code.Aborted);
    expect((await assets.setAssetHead({ projectId, revisionId: b, expectedHeadId: a })).previousHeadId).toBe(a);

    expect((await assets.getAssetRevision({ projectId })).revision?.id).toBe(b);
    const listed = await assets.listAssetRevisions({ projectId });
    expect(listed.headId).toBe(b);
    expect(listed.revisions.map((revision) => revision.id)).toEqual([b, a]);
  });

  test("blob and pack URLs serve only blobs of revisions the environment's deployments pin", async () => {
    const { projectId, environmentId } = await createEnvironment(h);
    const created = await h
      .client(ProjectService)
      .createEnvironment({ requestId: crypto.randomUUID(), projectId, name: "other" });
    const otherId = created.environment?.id ?? "";
    const pinned = assetRevision({ packs: { ui: "pinned pack" }, shared: { "a.txt": "pinned file" } });
    const unpinned = assetRevision({ packs: { ui: "unpinned pack" } });
    await uploadAssets(h, projectId, pinned);
    await uploadAssets(h, projectId, unpinned);
    await uploadRelease(h, projectId, "r1");
    await h.client(DeploymentService).deploy({
      requestId: crypto.randomUUID(),
      environmentId,
      releaseId: "r1",
      assetRevisionId: pinned.id,
    });

    const { message } = await desiredState(h.deps, environmentId);
    expect(message.assets).toMatchObject({ revisionId: pinned.id, blobUrlPrefix: `${h.url}/blobs/` });
    expect(message.assets?.manifest).toEqual(pinned.manifest);
    const digest = (revision: typeof pinned, text: string) =>
      [...revision.blobs].find(([, bytes]) => new TextDecoder().decode(bytes) === text)?.[0] ?? "";
    const [pinnedPack, pinnedFile, unpinnedPack] = [
      digest(pinned, "pinned pack"),
      digest(pinned, "pinned file"),
      digest(unpinned, "unpinned pack"),
    ];

    const token = await issueEnvironmentToken(h.db, environmentId);
    const otherToken = await issueEnvironmentToken(h.db, otherId);
    const blob = (sha256: string, bearer?: string) =>
      fetch(`${message.assets?.blobUrlPrefix}${sha256}`, {
        headers: bearer ? { authorization: `Bearer ${bearer}` } : {},
      });
    const served = await blob(pinnedFile, token);
    expect(served.status).toBe(200);
    expect(await served.text()).toBe("pinned file");
    expect((await blob(unpinnedPack, token)).status).toBe(404);
    expect((await blob(pinnedFile, otherToken)).status).toBe(404);
    expect((await blob(pinnedFile)).status).toBe(401);
    expect((await blob(pinnedFile, h.operatorToken)).status).toBe(401);

    const pack = (sha256: string) => fetch(`${message.packUrlPrefix}${sha256}`);
    const packResponse = await pack(pinnedPack);
    expect(packResponse.status).toBe(200);
    expect(packResponse.headers.get("cache-control")).toContain("immutable");
    expect(await packResponse.text()).toBe("pinned pack");
    expect((await pack(pinnedFile)).status).toBe(404);
    expect((await pack(unpinnedPack)).status).toBe(404);
    const otherPrefix = (await desiredState(h.deps, otherId)).message.packUrlPrefix;
    expect((await fetch(`${otherPrefix}${pinnedPack}`)).status).toBe(404);
  });
});
