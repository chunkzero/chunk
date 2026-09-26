import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { randomBytes } from "node:crypto";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { Code, createClient } from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-web";

import { start } from "../src/app.ts";
import { loadConfig } from "../src/config.ts";
import { randomToken } from "../src/crypto.ts";
import { connect } from "../src/db.ts";
import { EdgeService } from "../src/gen/chunk/management/v1/edge_pb.ts";
import { databaseUrl } from "./harness.ts";

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

  test("stop ends open streams instead of waiting for them", async () => {
    const url = new URL(databaseUrl ?? "");
    url.pathname = `/${database}`;
    const edgeToken = `chunk_${randomToken()}`;
    const app = await start(
      loadConfig({
        DATABASE_URL: url.toString(),
        CHUNK_SECRET_KEY: randomBytes(32).toString("base64"),
        CHUNK_EDGE_TOKEN: edgeToken,
        CHUNK_DATA_DIR: dataDir,
        HOST: "127.0.0.1",
        PORT: "0",
      }),
    );
    const transport = createConnectTransport({
      baseUrl: app.url.origin,
      useBinaryFormat: true,
      interceptors: [
        (next) => (request) => {
          request.header.set("authorization", `Bearer ${edgeToken}`);
          return next(request);
        },
      ],
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
});
