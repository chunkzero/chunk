import { randomBytes } from "node:crypto";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import type { DescService } from "@bufbuild/protobuf";
import { type Client, createClient } from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-web";

import { ensureOperatorToken } from "../src/auth/tokens.ts";
import { listenForChanges } from "../src/changes.ts";
import { deriveKeys, type Keys, randomToken } from "../src/crypto.ts";
import { connect, migrate, type Sql } from "../src/db.ts";
import type { Deps } from "../src/deps.ts";
import { DeploymentService } from "../src/gen/chunk/management/v1/deployments_pb.ts";
import { ProjectService } from "../src/gen/chunk/management/v1/projects_pb.ts";
import { localReleaseStore } from "../src/releases/local-store.ts";
import type { ReleaseStore } from "../src/releases/store.ts";
import { createHandler } from "../src/server.ts";
import { releaseArchive } from "./fixtures.ts";

/** Tests that need Postgres run only when this is set, for example to a Podman container's URL. */
export const databaseUrl = process.env.TEST_DATABASE_URL;

export interface Harness {
  sql: Sql;
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

/** Serves the API on a random port against a fresh schema, a temporary release directory and a fake resolver. */
export async function startHarness(): Promise<Harness> {
  if (!databaseUrl) throw new Error("TEST_DATABASE_URL is not set");
  const schema = `test_${randomBytes(6).toString("hex")}`;
  const admin = connect(databaseUrl);
  await admin.unsafe(`create schema ${schema}`);
  const sql = connect(databaseUrl, { searchPath: schema });
  await migrate(sql);
  const operatorToken = `chunk_${randomToken()}`;
  await ensureOperatorToken(sql, operatorToken);
  const keys = deriveKeys(randomBytes(32));
  const directory = await mkdtemp(join(tmpdir(), "chunk-management-"));
  const txt = new Map<string, string[]>();

  const changes = await listenForChanges(sql);

  let handler: ReturnType<typeof createHandler> = async () => new Response(null, { status: 503 });
  const server = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch: (request, bun) => handler(request, bun) });
  const url = server.url.origin;
  const releases = await localReleaseStore({ directory, keys, publicUrl: url });
  const deps: Deps = {
    sql,
    keys,
    releases: {
      ...releases,
      async read(key) {
        await harness.beforeRead?.();
        return releases.read(key);
      },
    },
    archiveLimits: { maxExpandedBytes: 64 * 1024 * 1024, maxEntries: 1000 },
    async resolveTxt(name) {
      const records = txt.get(name);
      if (!records) throw Object.assign(new Error(`no records for ${name}`), { code: "ENOTFOUND" });
      return records.map((record) => [record]);
    },
    publicUrl: url,
    edge: { domain: "play.example.net", port: 25565 },
    logStore: undefined,
    changes,
  };
  const harness: Harness = {
    sql,
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
  handler = createHandler(deps);

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
    await sql.end();
    await admin.unsafe(`drop schema ${schema} cascade`);
    await admin.end();
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

/** Uploads a fixture release and deploys it; returns the deployment ID. */
export async function deployRelease(h: Harness, projectId: string, environmentId: string, releaseId: string) {
  const deployments = h.client(DeploymentService);
  const archive = releaseArchive(releaseId);
  const started = await deployments.uploadRelease({
    projectId,
    releaseId,
    archiveSha256: archive.sha256,
    archiveSizeBytes: archive.sizeBytes,
  });
  await fetch(started.upload?.url ?? "", { method: "PUT", body: archive.bytes });
  await deployments.completeReleaseUpload({ projectId, releaseId });
  const deployed = await deployments.deploy({ requestId: crypto.randomUUID(), environmentId, releaseId });
  return deployed.deployment?.id ?? "";
}
