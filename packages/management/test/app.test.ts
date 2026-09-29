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
import { createConnectTransport } from "@connectrpc/connect-web";

import { start } from "../src/app.ts";
import { loadConfig } from "../src/config.ts";
import { randomToken } from "../src/crypto.ts";
import { connect } from "../src/db.ts";
import { EdgeService } from "../src/gen/chunk/management/v1/edge_pb.ts";
import { ProjectService } from "../src/gen/chunk/management/v1/projects_pb.ts";
import { subjectOf } from "../src/rpc/caller.ts";
import { fakeProvider } from "./fake-provider.ts";
import { codeOf, databaseUrl } from "./harness.ts";

/** A registry holding `test.v1.WhoAmIService`, whose `WhoAmI` answers with a string. */
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
    ],
  }),
  (name) => [file_google_protobuf_empty, file_google_protobuf_wrappers].find((file) => file.proto.name === name),
);
const WhoAmIService: GenService<{
  whoAmI: { methodKind: "unary"; input: typeof EmptySchema; output: typeof StringValueSchema };
}> = serviceDesc(registry.getFile("test/v1/whoami.proto") ?? expect.unreachable(), 0);

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

  test("serves an install's provider, migrations, services and credentials", async () => {
    const migrations = join(dataDir, "migrations");
    await Bun.write(join(migrations, "0001_init.sql"), "create table extension_greetings (text text not null);");
    await Bun.write(join(migrations, "0002_greet.sql"), "insert into extension_greetings values ('hello');");
    const { provider } = fakeProvider();
    let installId = "";
    const { promise: listed, resolve: list } = Promise.withResolvers<void>();
    let stopped = false;
    const app = await start(config({ CHUNK_ENVIRONMENT_IMAGE: "chunk-environment:test" }), {
      provider(_deps, id) {
        installId = id;
        return { ...provider, list: () => (list(), provider.list()) };
      },
      migrations,
      extend: (router, deps) =>
        router.service(WhoAmIService, {
          async whoAmI(_request, context) {
            const [greeting] = await deps.sql<{ text: string }[]>`select text from extension_greetings`;
            return { value: `${greeting?.text} ${subjectOf(context)}` };
          },
        }),
      authenticator: (tokens) => ({
        authenticate: async (token) =>
          token === "agent-secret"
            ? { kind: "extension", service: WhoAmIService.typeName, subject: "agent-1" }
            : tokens.authenticate(token),
      }),
      start: async () => async () => {
        stopped = true;
      },
    });
    const client = <T extends Parameters<typeof createClient>[0]>(service: T, token: string) =>
      createClient(
        service,
        createConnectTransport({ baseUrl: app.url.origin, useBinaryFormat: true, interceptors: [bearer(token)] }),
      );

    expect(await client(WhoAmIService, "agent-secret").whoAmI({})).toMatchObject({ value: "hello agent-1" });
    expect(await codeOf(client(WhoAmIService, "wrong").whoAmI({}))).toBe(Code.Unauthenticated);
    expect(await codeOf(client(ProjectService, "agent-secret").listProjects({}))).toBe(Code.PermissionDenied);
    await listed;
    expect(installId).toMatch(/.+/);

    await app.stop();
    expect(stopped).toBe(true);
  });
});
