import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { randomBytes } from "node:crypto";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { create, createFileRegistry } from "@bufbuild/protobuf";
import { type GenService, serviceDesc } from "@bufbuild/protobuf/codegenv2";
import {
  type EmptySchema,
  FileDescriptorProtoSchema,
  file_google_protobuf_empty,
  file_google_protobuf_wrappers,
  type StringValueSchema,
} from "@bufbuild/protobuf/wkt";
import { Code, createClient, type Interceptor } from "@connectrpc/connect";
import { createConnectTransport, createGrpcWebTransport } from "@connectrpc/connect-web";

import { start } from "../src/app.ts";
import { loadConfig } from "../src/config.ts";
import { randomToken } from "../src/crypto.ts";
import { connect } from "../src/db.ts";
import { AuthService } from "../src/gen/chunk/management/v1/auth_pb.ts";
import { EdgeService } from "../src/gen/chunk/management/v1/edge_pb.ts";
import { ProjectService } from "../src/gen/chunk/management/v1/projects_pb.ts";
import { type Identity, subjectOf } from "../src/rpc/caller.ts";
import { fakeProvider } from "./fake-provider.ts";
import { codeOf, databaseUrl } from "./harness.ts";

/** A registry holding `test.v1.WhoAmIService` and `test.v1.PingService`, whose methods answer with a string. */
const registry = createFileRegistry(
  create(FileDescriptorProtoSchema, {
    name: "test/v1/whoami.proto",
    package: "test.v1",
    dependency: ["google/protobuf/empty.proto", "google/protobuf/wrappers.proto"],
    service: [
      {
        name: "WhoAmIService",
        method: [{ name: "WhoAmI", inputType: ".google.protobuf.Empty", outputType: ".google.protobuf.StringValue" }],
      },
      {
        name: "PingService",
        method: [{ name: "Ping", inputType: ".google.protobuf.Empty", outputType: ".google.protobuf.StringValue" }],
      },
    ],
  }),
  (name) => [file_google_protobuf_empty, file_google_protobuf_wrappers].find((file) => file.proto.name === name),
);
const WhoAmIService: GenService<{
  whoAmI: { methodKind: "unary"; input: typeof EmptySchema; output: typeof StringValueSchema };
}> = serviceDesc(registry.getFile("test/v1/whoami.proto") ?? expect.unreachable(), 0);
const PingService: GenService<{
  ping: { methodKind: "unary"; input: typeof EmptySchema; output: typeof StringValueSchema };
}> = serviceDesc(registry.getFile("test/v1/whoami.proto") ?? expect.unreachable(), 1);

/** A registration's own interceptors, which replace the router's. */
const passThrough: Interceptor = (next) => (request) => next(request);

const bearer =
  (token: string): Interceptor =>
  (next) =>
  (request) => {
    request.header.set("authorization", `Bearer ${token}`);
    return next(request);
  };

describe.skipIf(!databaseUrl)("start", () => {
  const database = `test_app_${randomBytes(6).toString("hex")}`;
  let admin: ReturnType<typeof connect>;
  let dataDir: string;
  beforeAll(async () => {
    admin = connect(databaseUrl ?? "");
    await admin.unsafe(`create database ${database}`);
    dataDir = await mkdtemp(join(tmpdir(), "chunk-management-app-"));
  });
  afterAll(async () => {
    await admin.unsafe(`drop database if exists ${database} with (force)`);
    await admin.end();
    await rm(dataDir, { recursive: true, force: true });
  });

  function config(extra: Record<string, string> = {}) {
    const url = new URL(databaseUrl ?? "");
    url.pathname = `/${database}`;
    return loadConfig({
      DATABASE_URL: url.toString(),
      CHUNK_SECRET_KEY: randomBytes(32).toString("base64"),
      CHUNK_DATA_DIR: dataDir,
      HOST: "127.0.0.1",
      PORT: "0",
      ...extra,
    });
  }

  test("stop ends open streams instead of waiting for them", async () => {
    const edgeToken = `chunk_${randomToken()}`;
    const app = await start(config({ CHUNK_EDGE_TOKEN: edgeToken }));
    const transport = createConnectTransport({
      baseUrl: app.url.origin,
      useBinaryFormat: true,
      interceptors: [bearer(edgeToken)],
    });
    const routes = createClient(EdgeService, transport).watchRoutes({})[Symbol.asyncIterator]();
    expect((await routes.next()).value.reset).toBe(true);

    const stopped = app.stop().then(() => "stopped");
    const timeout = new Promise((resolve) => setTimeout(() => resolve("timed out"), 5000));
    expect(await Promise.race([stopped, timeout])).toBe("stopped");
    const ended = await routes.next().then(
      () => undefined,
      (error: { code?: number }) => error.code,
    );
    expect(ended).toBe(Code.Unavailable);
  });

  /** Whether every connection to the test database has closed, waiting up to five seconds. */
  async function closed() {
    for (let attempt = 0; attempt < 50; attempt++) {
      const [row] = await admin<{ count: number }[]>`
        select count(*)::int as count from pg_stat_activity where datname = ${database}`;
      if (row?.count === 0) return true;
      await Bun.sleep(100);
    }
    return false;
  }

  /** The fake provider, and a promise that settles once the reconciler, holding its leader lock, lists machines. */
  function listedProvider() {
    const { provider } = fakeProvider();
    const { promise: listed, resolve } = Promise.withResolvers<void>();
    return { listed, provider: { ...provider, list: () => (resolve(), provider.list()) } };
  }

  test("serves an install's provider, migrations, services and credentials", async () => {
    const migrations = join(dataDir, "migrations");
    await Bun.write(join(migrations, "0001_init.sql"), "create table extension_greetings (text text not null);");
    await Bun.write(join(migrations, "0002_greet.sql"), "insert into extension_greetings values ('hello');");
    const { listed, provider } = listedProvider();
    const credentials = new Map<string, Identity>([
      ["agent", { kind: "extension", service: WhoAmIService.typeName, subject: "agent-1" }],
      ["pinger", { kind: "extension", service: PingService.typeName, subject: "agent-4" }],
      ["other", { kind: "extension", service: "test.v1.OtherService", subject: "agent-2" }],
      ["edge", { kind: "extension", service: EdgeService.typeName, subject: "agent-3" }],
    ]);
    const operatorToken = `chunk_${randomToken()}`;
    let installId = "";
    let handled = 0;
    const app = await start(
      config({ CHUNK_ENVIRONMENT_IMAGE: "chunk-environment:test", CHUNK_OPERATOR_TOKEN: operatorToken }),
      {
        provider(_deps, id) {
          installId = id;
          return provider;
        },
        migrations,
        extend: (router, deps) =>
          router
            .service(
              WhoAmIService,
              {
                async whoAmI(_request, context) {
                  handled++;
                  const [greeting] = await deps.sql<{ text: string }[]>`select text from extension_greetings`;
                  return { value: `${greeting?.text} ${subjectOf(context)}` };
                },
              },
              { interceptors: [passThrough] },
            )
            .rpc(
              PingService.method.ping,
              (_request, context) => {
                handled++;
                return { value: subjectOf(context) };
              },
              { interceptors: [passThrough] },
            )
            .service(
              EdgeService,
              {
                wake() {
                  handled++;
                  return {};
                },
              },
              { interceptors: [passThrough] },
            ),
        authenticator: (tokens) => ({
          authenticate: async (token) => credentials.get(token) ?? tokens.authenticate(token),
        }),
        start: async () => async () => {
          throw new Error("stop failed");
        },
      },
    );
    const client = <T extends Parameters<typeof createClient>[0]>(service: T, token?: string) =>
      createClient(
        service,
        createConnectTransport({
          baseUrl: app.url.origin,
          useBinaryFormat: true,
          interceptors: token === undefined ? [] : [bearer(token)],
        }),
      );
    const project = await client(ProjectService, operatorToken).createProject({
      requestId: crypto.randomUUID(),
      name: "extension-test",
    });
    const { secret: projectToken } = await client(AuthService, operatorToken).createToken({
      requestId: crypto.randomUUID(),
      name: "project",
      projectId: project.project?.id ?? "",
    });

    expect(await client(WhoAmIService, "agent").whoAmI({})).toMatchObject({ value: "hello agent-1" });
    expect(await client(PingService, "pinger").ping({})).toMatchObject({ value: "agent-4" });
    for (const [token, code] of [
      [undefined, Code.Unauthenticated],
      ["wrong", Code.Unauthenticated],
      [operatorToken, Code.PermissionDenied],
      [projectToken, Code.PermissionDenied],
      ["other", Code.PermissionDenied],
    ] as const) {
      expect(await codeOf(client(WhoAmIService, token).whoAmI({}))).toBe(code);
      expect(await codeOf(client(PingService, token).ping({}))).toBe(code);
    }
    for (const [token, code] of [
      [undefined, Code.Unauthenticated],
      ["edge", Code.PermissionDenied],
    ] as const) {
      const edge = client(EdgeService, token);
      expect(await codeOf(edge.watchRoutes({})[Symbol.asyncIterator]().next())).toBe(code);
      expect(await codeOf(edge.wake({ environmentId: "env_missing" }))).toBe(code);
    }
    expect(await codeOf(client(ProjectService, "agent").listProjects({}))).toBe(Code.PermissionDenied);
    // Refusals answer in the client's protocol. Bun serves no HTTP/2, so plain gRPC cannot be observed here.
    const grpcWeb = createClient(WhoAmIService, createGrpcWebTransport({ baseUrl: app.url.origin }));
    expect(await codeOf(grpcWeb.whoAmI({}))).toBe(Code.Unauthenticated);
    expect(handled).toBe(2);
    await listed;
    expect(installId).toMatch(/.+/);

    await expect(app.stop()).rejects.toThrow("stop failed");
    expect(await closed()).toBe(true);
  });

  test("stops what it started when a start hook fails", async () => {
    const { listed, provider } = listedProvider();
    const started = start(config({ CHUNK_ENVIRONMENT_IMAGE: "chunk-environment:test" }), {
      provider: () => provider,
      async start() {
        await listed;
        throw new Error("start failed");
      },
    });
    await expect(started).rejects.toThrow("start failed");
    expect(await closed()).toBe(true);
  });
});
