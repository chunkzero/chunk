import { afterAll, beforeAll, describe, expect, test } from "bun:test";

import { Code } from "@connectrpc/connect";

import { collectAssets, keptRecentDeployments, keptRecentRevisions } from "../src/assets/retention.ts";
import { decodeRevision, sha256Hex } from "../src/assets/revision.ts";
import { issueEnvironmentToken } from "../src/auth/tokens.ts";
import { desiredState } from "../src/environments/desired.ts";
import { AssetRevisionState, AssetService } from "../src/gen/chunk/management/v1/assets_pb.ts";
import { DeploymentState } from "../src/gen/chunk/management/v1/common_pb.ts";
import { DeploymentService } from "../src/gen/chunk/management/v1/deployments_pb.ts";
import { EnvironmentService } from "../src/gen/chunk/management/v1/environment_pb.ts";
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
    const { exists, complete } = h.deps.releases;
    h.deps.releases.exists = async (key) => key !== lostKey && exists(key);
    h.deps.releases.complete = async (key, expected, verify) =>
      key === lostKey ? undefined : complete(key, expected, verify);
    try {
      const { uploads } = await assets.uploadAssets({ projectId, manifest: revision.manifest });
      expect(uploads.map((upload) => upload.sha256)).toEqual([sha256]);
      expect(await codeOf(assets.completeAssetUpload({ projectId, revisionId: revision.id }))).toBe(
        Code.FailedPrecondition,
      );
    } finally {
      h.deps.releases.exists = exists;
      h.deps.releases.complete = complete;
    }
  });

  test("a ready revision whose blob was lost is repaired by uploading the blob again", async () => {
    const { projectId } = await createEnvironment(h);
    const assets = h.client(AssetService);
    const revision = assetRevision({ shared: { "a.txt": "a" } });
    await uploadAssets(h, projectId, revision);
    const sha256 = [...revision.blobs.keys()][0] ?? "";
    const lostKey = blobKey(projectId, sha256);
    const { exists } = h.deps.releases;
    let repaired = false;
    h.deps.releases.exists = async (key) => (key === lostKey && !repaired ? false : exists(key));
    try {
      const { uploads } = await assets.uploadAssets({ projectId, manifest: revision.manifest });
      const target = uploads[0]?.upload?.url ?? "";
      await fetch(target, { method: "PUT", body: revision.blobs.get(sha256) });
      repaired = true;
      const completed = await assets.completeAssetUpload({ projectId, revisionId: revision.id });
      expect(completed.revision?.state).toBe(AssetRevisionState.READY);
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

  test("collection keeps the head, recent and deployed revisions, and deletes the rest with their blobs", async () => {
    const { projectId, environmentId } = await createEnvironment(h);
    const assets = h.client(AssetService);
    await uploadRelease(h, projectId, "r1");
    const deployed = assetRevision({ shared: { "d.txt": "deployed", "f.txt": "r0" } });
    await uploadAssets(h, projectId, deployed);
    await h.client(DeploymentService).deploy({
      requestId: crypto.randomUUID(),
      environmentId,
      releaseId: "r1",
      assetRevisionId: deployed.id,
    });
    const recent = Array.from({ length: keptRecentRevisions + 2 }, (_, i) =>
      assetRevision({ shared: { "f.txt": `r${i}` } }),
    );
    for (const revision of recent) await uploadAssets(h, projectId, revision);
    const unfinished = assetRevision({ shared: { "u.txt": "unfinished" } });
    const [upload] = (await assets.uploadAssets({ projectId, manifest: unfinished.manifest })).uploads;
    await fetch(upload?.upload?.url ?? "", { method: "PUT", body: unfinished.blobs.get(upload?.sha256 ?? "") });
    await h.sql`update asset_revisions set protected_until = now() - interval '1 minute' where project_id = ${projectId}`;
    const head = recent[0]?.id ?? "";
    await assets.setAssetHead({ projectId, revisionId: head, expectedHeadId: "" });

    const remaining = await h.sql<{ id: string }[]>`select id from asset_revisions where project_id = ${projectId}`;
    const kept = [deployed, ...recent.slice(0, 1), ...recent.slice(2)].map((revision) => revision.id);
    expect(remaining.map((row) => row.id).sort()).toEqual(kept.sort());
    const stored = (revision: typeof deployed) =>
      Promise.all([...revision.blobs.keys()].map((sha256) => h.releases.exists(blobKey(projectId, sha256))));
    expect(await stored(recent[0] ?? deployed)).toEqual([true]);
    expect(await stored(recent[1] ?? deployed)).toEqual([false]);
    expect(await stored(unfinished)).toEqual([false]);
    expect(await assets.uploadAssets({ projectId, manifest: recent[1]?.manifest ?? new Uint8Array() })).toMatchObject({
      uploads: [{ sha256: [...(recent[1]?.blobs.keys() ?? [])][0] }],
    });
  });

  /**
   * An environment whose project holds an old revision, `old`, and enough newer ones and unfinished deployments that
   * neither the recent-revision nor the recent-deployment rule keeps anything of `old`, with a core attached to it.
   */
  async function aged() {
    const { projectId, environmentId } = await createEnvironment(h);
    const deploy = h.client(DeploymentService);
    await uploadRelease(h, projectId, "r1");
    const [first, second] = [
      assetRevision({ shared: { "f.txt": "first" } }),
      assetRevision({ shared: { "f.txt": "second" } }),
    ];
    for (const revision of [first, second]) await uploadAssets(h, projectId, revision);
    const fillers = Array.from({ length: keptRecentRevisions + 1 }, (_, i) =>
      assetRevision({ shared: { "f.txt": `filler${i}` } }),
    );
    for (const filler of fillers) await uploadAssets(h, projectId, filler);
    await h.sql`update projects set asset_head_id = ${fillers[0]?.id ?? ""} where id = ${projectId}`;
    const pin = async (assetRevisionId: string) =>
      (await deploy.deploy({ requestId: crypto.randomUUID(), environmentId, releaseId: "r1", assetRevisionId }))
        .deployment?.id ?? "";
    const client = h.client(EnvironmentService, await issueEnvironmentToken(h.db, environmentId));
    const abort = new AbortController();
    const stream = client.attach({ instanceId: crypto.randomUUID(), core: true, epoch: 1n }, { signal: abort.signal });
    const { lease: attachedLease, revision } = (await stream[Symbol.asyncIterator]().next()).value;
    let sequence = 0n;
    let lease = attachedLease;
    return {
      projectId,
      environmentId,
      first,
      second,
      fillers,
      pin,
      /** Pins `fillers[0]` enough times that the deployments before them are none of the newest. */
      bury: async () => {
        for (let i = 0; i < keptRecentDeployments + 1; i++) await pin(fillers[0]?.id ?? "");
        await h.sql`update asset_revisions set protected_until = now() - interval '1 minute' where project_id = ${projectId}`;
      },
      report: (resident: string[], deployment?: { deploymentId: string; state: DeploymentState }, known = true) =>
        client.reportStatus({
          lease,
          sequence: ++sequence,
          desiredRevision: revision,
          residentDeploymentIds: resident,
          residentDeploymentsKnown: known,
          ...(deployment ? { deployment } : {}),
        }),
      /** Attaches the core again, which starts a new lease whose reports haven't read the resident set. */
      reattach: async () => {
        const next = client.attach(
          { instanceId: crypto.randomUUID(), core: true, epoch: 2n },
          { signal: abort.signal },
        );
        const first = (await next[Symbol.asyncIterator]().next()).value;
        lease = first.lease;
        sequence = 0n;
      },
      /** A report sent now whose arrival is delayed until the returned function is called. */
      delayed: (resident: string[], deployment: { deploymentId: string; state: DeploymentState }) => {
        const delayedSequence = ++sequence;
        return () =>
          client.reportStatus({
            lease,
            sequence: delayedSequence,
            desiredRevision: revision,
            residentDeploymentIds: resident,
            residentDeploymentsKnown: true,
            deployment,
          });
      },
      stored: async (id: string) =>
        (await h.sql<{ id: string }[]>`select id from asset_revisions where project_id = ${projectId} and id = ${id}`)
          .length > 0,
      close: () => abort.abort(),
    };
  }

  test("a deployment core activated, whose report is delayed past newer deployments, activates with its revision present", async () => {
    const env = await aged();
    const active = await env.pin(env.first.id);
    await env.report([active], { deploymentId: active, state: DeploymentState.ACTIVE });
    const activated = await env.pin(env.second.id);
    const lateReport = env.delayed([active, activated], { deploymentId: activated, state: DeploymentState.ACTIVE });
    await env.bury();

    await collectAssets(h.deps, env.projectId);
    expect(await env.stored(env.second.id)).toBe(true);

    await lateReport();
    const { deployment } = await h.client(DeploymentService).getDeployment({ deploymentId: activated });
    expect(deployment?.state).toBe(DeploymentState.ACTIVE);
    expect(await env.stored(deployment?.assetRevisionId ?? "")).toBe(true);
    env.close();
  });

  test("a superseded deployment in core's reported resident set keeps its revision until a report omits it", async () => {
    const env = await aged();
    const old = await env.pin(env.first.id);
    await env.report([old], { deploymentId: old, state: DeploymentState.ACTIVE });
    const current = await env.pin(env.second.id);
    await env.report([old, current], { deploymentId: current, state: DeploymentState.ACTIVE });
    await env.bury();

    await collectAssets(h.deps, env.projectId);
    expect(await env.stored(env.first.id)).toBe(true);

    await env.report([current]);
    await collectAssets(h.deps, env.projectId);
    expect(await env.stored(env.first.id)).toBe(false);
    expect(await env.stored(env.second.id)).toBe(true);
    env.close();
  });

  test("a report that didn't read the resident set keeps the stored set and every revision of the environment", async () => {
    const env = await aged();
    const old = await env.pin(env.first.id);
    await env.report([old], { deploymentId: old, state: DeploymentState.ACTIVE });
    const current = await env.pin(env.second.id);
    await env.report([current], { deploymentId: current, state: DeploymentState.ACTIVE });
    await env.bury();

    await env.report([], undefined, false);
    await collectAssets(h.deps, env.projectId);
    expect(await env.stored(env.first.id)).toBe(true);
    expect(
      await h.sql<
        { resident_deployment_ids: string[] }[]
      >`select resident_deployment_ids from environments where id = ${env.environmentId}`,
    ).toEqual([{ resident_deployment_ids: [current] }]);

    await env.report([current]);
    await collectAssets(h.deps, env.projectId);
    expect(await env.stored(env.first.id)).toBe(false);
    env.close();
  });

  test("a new lease keeps every revision of the environment until a report reads the resident set", async () => {
    const env = await aged();
    const old = await env.pin(env.first.id);
    await env.report([old], { deploymentId: old, state: DeploymentState.ACTIVE });
    const current = await env.pin(env.second.id);
    await env.report([current], { deploymentId: current, state: DeploymentState.ACTIVE });
    await env.bury();

    await env.reattach();
    await collectAssets(h.deps, env.projectId);
    expect(await env.stored(env.first.id)).toBe(true);

    await env.report([current]);
    await collectAssets(h.deps, env.projectId);
    expect(await env.stored(env.first.id)).toBe(false);
    env.close();
  });

  test("a completion paused after verifying its blobs holds off collection, so the revision does not become ready without them", async () => {
    const { projectId } = await createEnvironment(h);
    const assets = h.client(AssetService);
    const revision = assetRevision({ shared: { "a.txt": "a" } });
    const [upload] = (await assets.uploadAssets({ projectId, manifest: revision.manifest })).uploads;
    await fetch(upload?.upload?.url ?? "", { method: "PUT", body: revision.blobs.get(upload?.sha256 ?? "") });
    await h.sql`update asset_revisions set protected_until = now() - interval '1 minute' where project_id = ${projectId}`;

    const { complete } = h.deps.releases;
    let verified: () => void = () => {};
    const verifiedSignal = new Promise<void>((resolve) => (verified = resolve));
    let resume: () => void = () => {};
    const gate = new Promise<void>((resolve) => (resume = resolve));
    h.deps.releases.complete = async (key, expected, verify) => {
      const result = await complete(key, expected, verify);
      verified();
      await gate;
      return result;
    };
    try {
      const completing = assets.completeAssetUpload({ projectId, revisionId: revision.id });
      await verifiedSignal;
      const state = { collected: false };
      const collecting = collectAssets(h.deps, projectId).finally(() => (state.collected = true));
      // Collection is blocked behind the completion's shared lock.
      while (!state.collected) {
        const [row] = await h.sql<{ waiting: number }[]>`
          select count(*)::int as waiting from pg_locks where locktype = 'advisory' and not granted`;
        if (row && row.waiting > 0) break;
        await Bun.sleep(10);
      }
      resume();
      expect((await completing).revision?.state).toBe(AssetRevisionState.READY);
      expect(await collecting).toEqual({ revisions: 0, blobs: 0 });
    } finally {
      resume();
      h.deps.releases.complete = complete;
    }
    const sha256 = [...revision.blobs.keys()][0] ?? "";
    expect(await h.releases.exists(blobKey(projectId, sha256))).toBe(true);
    expect((await assets.getAssetRevision({ projectId, revisionId: revision.id })).revision?.state).toBe(
      AssetRevisionState.READY,
    );
  });

  test("a recorded deletion is dropped when its blob is referenced again, and retried when the store fails", async () => {
    const { projectId } = await createEnvironment(h);
    const [referenced, lost, flaky] = [1, 2, 3].map((i) => assetRevision({ shared: { "f.txt": `blob${i}` } }));
    for (const revision of [referenced, lost, flaky]) await uploadAssets(h, projectId, revision);
    const digest = (revision?: { blobs: Map<string, unknown> }) => [...(revision?.blobs.keys() ?? [])][0] ?? "";
    for (const revision of [lost, flaky]) {
      await h.sql`delete from asset_revisions where project_id = ${projectId} and id = ${revision?.id ?? ""}`;
      await h.sql`delete from asset_blobs where project_id = ${projectId} and sha256 = ${digest(revision)}`;
    }
    for (const revision of [referenced, lost, flaky]) {
      await h.sql`insert into asset_deletions (project_id, sha256) values (${projectId}, ${digest(revision)})`;
    }
    const recorded = async () =>
      (await h.sql<{ sha256: string }[]>`select sha256 from asset_deletions where project_id = ${projectId}`).map(
        (row) => row.sha256,
      );
    const present = (revision?: { blobs: Map<string, unknown> }) =>
      h.releases.exists(blobKey(projectId, digest(revision)));
    const { delete: remove } = h.deps.releases;
    const flakyKey = blobKey(projectId, digest(flaky));
    h.deps.releases.delete = async (key) => {
      if (key === flakyKey) throw new Error("store unavailable");
      await remove(key);
    };
    try {
      await collectAssets(h.deps, projectId);
      expect(await recorded()).toEqual([digest(flaky)]);
      expect([await present(referenced), await present(lost), await present(flaky)]).toEqual([true, false, true]);
    } finally {
      h.deps.releases.delete = remove;
    }
    await collectAssets(h.deps, projectId);
    expect(await recorded()).toEqual([]);
    expect(await present(flaky)).toBe(false);
  });
});
