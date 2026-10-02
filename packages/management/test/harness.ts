import { randomBytes } from "node:crypto";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import type { DescService } from "@bufbuild/protobuf";
import { type Client, createClient } from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-web";
import { SQL } from "bun";

import { ensureOperatorToken } from "../src/auth/tokens.ts";
import { listenForChanges } from "../src/changes.ts";
import { deriveKeys, type Keys, randomToken } from "../src/crypto.ts";
import { connect, type Database, migrate } from "../src/db.ts";
import type { Deps } from "../src/deps.ts";
import { AssetService } from "../src/gen/chunk/management/v1/assets_pb.ts";
import { DeploymentService } from "../src/gen/chunk/management/v1/deployments_pb.ts";
import { ProjectService } from "../src/gen/chunk/management/v1/projects_pb.ts";
import { localReleaseStore } from "../src/releases/local-store.ts";
import type { ReleaseStore } from "../src/releases/store.ts";
import { createHandler, type HandlerOptions } from "../src/server.ts";
import { assetRevision, releaseArchive } from "./fixtures.ts";

/** Tests that need Postgres run only when this is set, for example to a Podman container's URL. */
export const databaseUrl = process.env.TEST_DATABASE_URL;

/** The reconciler's production defaults. */
export const reconcilerLimits = {
  concurrency: 8,
  timeouts: { startMs: 120_000, callMs: 60_000 },
  capacityRetryMs: 300_000,
};

export interface Harness {
  db: Database;
  /** The Bun.SQL client underneath `db`, for tests that read or change rows directly. */
  sql: SQL;
  keys: Keys;
  /** What the handler was built with, for driving background work such as the reconciler directly. */
  deps: Deps;
  url: string;
  operatorToken: string;
  /** TXT records the fake resolver answers with, by name. */
  txt: Map<string, string[]>;
  releases: ReleaseStore;
  /** Runs before the service reads a stored archive, to interleave other calls with verification. */
  beforeRead: (() => Promise<void>) | undefined;
  /** A client sending `token` as its bearer, or none when null. */
  client<T extends DescService>(service: T, token?: string | null): Client<T>;
  /** Calls the same handler directly so tests can observe consumption of a request body. */
  fetch(request: Request): Promise<Response>;
  close(): Promise<void>;
}

/**
 * Serves the API on a random port against a fresh database, a temporary release directory and a fake resolver, with
 * `overrides` replacing the defaults and the handler built with `options`.
 */
export async function startHarness(overrides: Partial<Deps> = {}, options: HandlerOptions = {}): Promise<Harness> {
  if (!databaseUrl) throw new Error("TEST_DATABASE_URL is not set");
  const database = `test_${randomBytes(6).toString("hex")}`;
  const admin = new SQL(databaseUrl);
  await admin.unsafe(`create database ${database}`);
  const location = new URL(databaseUrl);
  location.pathname = `/${database}`;
  const db = connect(location.toString());
  await migrate(db);
  const operatorToken = `chunk_${randomToken()}`;
  await ensureOperatorToken(db, operatorToken);
  const keys = deriveKeys(randomBytes(32));
  const directory = await mkdtemp(join(tmpdir(), "chunk-management-"));
  const txt = new Map<string, string[]>();

  const changes = await listenForChanges(db);

  let handler: ReturnType<typeof createHandler> = async () => new Response(null, { status: 503 });
  const server = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch: (request, bun) => handler(request, bun) });
  const url = server.url.origin;
  const releases = await localReleaseStore({ directory, keys, publicUrl: url, machineUrl: url });
  const deps: Deps = {
    db,
    keys,
    releases: {
      ...releases,
      async complete(key, expected, verify) {
        await harness.beforeRead?.();
        return releases.complete(key, expected, verify);
      },
    },
    archiveLimits: { maxExpandedBytes: 64 * 1024 * 1024, maxEntries: 1000 },
    async resolveTxt(name) {
      const records = txt.get(name);
      if (!records) throw Object.assign(new Error(`no records for ${name}`), { code: "ENOTFOUND" });
      return records.map((record) => [record]);
    },
    publicUrl: url,
    machineUrl: url,
    edge: { domain: "play.example.net", port: 25565 },
    logStore: undefined,
    jvmImage: "chunk-jvm:{java}",
    changes,
    shutdown: new AbortController().signal,
    ...overrides,
  };
  const harness: Harness = {
    db,
    sql: db.$client,
    keys,
    deps,
    url,
    operatorToken,
    txt,
    releases,
    beforeRead: undefined,
    client,
    fetch: (request) => handler(request),
    close,
  };
  handler = createHandler(deps, options);

  function client<T extends DescService>(service: T, token: string | null = operatorToken): Client<T> {
    const transport = createConnectTransport({
      baseUrl: url,
      useBinaryFormat: true,
      interceptors:
        token === null
          ? []
          : [
              (next) => (request) => {
                request.header.set("authorization", `Bearer ${token}`);
                return next(request);
              },
            ],
    });
    return createClient(service, transport);
  }

  async function close() {
    await server.stop(true);
    await db.$client.close();
    await admin.unsafe(`drop database ${database} with (force)`);
    await admin.close();
    await rm(directory, { recursive: true, force: true });
  }

  return harness;
}

/** Runs `call` and returns the Connect error code it failed with. */
export async function codeOf(call: Promise<unknown>): Promise<number | undefined> {
  try {
    await call;
    return undefined;
  } catch (error) {
    return (error as { code?: number }).code;
  }
}

/** Creates a project holding one environment. */
export async function createEnvironment(h: Harness, name = "main") {
  const projects = h.client(ProjectService);
  const project = await projects.createProject({
    requestId: crypto.randomUUID(),
    name: `game-${randomBytes(4).toString("hex")}`,
  });
  const projectId = project.project?.id ?? "";
  const created = await projects.createEnvironment({ requestId: crypto.randomUUID(), projectId, name });
  return { projectId, environmentId: created.environment?.id ?? "" };
}

/** Uploads a release, a fixture one by default, and completes the upload. */
export async function uploadRelease(
  h: Harness,
  projectId: string,
  releaseId: string,
  archive = releaseArchive(releaseId),
) {
  const deployments = h.client(DeploymentService);
  const started = await deployments.uploadRelease({
    projectId,
    releaseId,
    archiveSha256: archive.sha256,
    archiveSizeBytes: archive.sizeBytes,
  });
  await fetch(started.upload?.url ?? "", { method: "PUT", body: archive.bytes });
  await deployments.completeReleaseUpload({ projectId, releaseId });
}

/** Uploads an asset revision, an empty one by default, with all its blobs, and completes it; returns its ID. */
export async function uploadAssets(h: Harness, projectId: string, revision = assetRevision()) {
  const assets = h.client(AssetService);
  const { uploads } = await assets.uploadAssets({ projectId, manifest: revision.manifest });
  for (const { sha256, upload } of uploads) {
    await fetch(upload?.url ?? "", { method: "PUT", body: revision.blobs.get(sha256) ?? new Uint8Array() });
  }
  await assets.completeAssetUpload({ projectId, revisionId: revision.id });
  return revision.id;
}

/** Uploads a fixture release and an empty asset revision, and deploys them; returns the deployment ID. */
export async function deployRelease(h: Harness, projectId: string, environmentId: string, releaseId: string) {
  await uploadRelease(h, projectId, releaseId);
  const assetRevisionId = await uploadAssets(h, projectId);
  const deployed = await h
    .client(DeploymentService)
    .deploy({ requestId: crypto.randomUUID(), environmentId, releaseId, assetRevisionId });
  return deployed.deployment?.id ?? "";
}

/** The stream's next message; fails when the stream ended instead. */
export async function next<T>(messages: AsyncIterator<T>): Promise<T> {
  const result = await messages.next();
  if (result.done) throw new Error("the stream ended");
  return result.value;
}
