import { afterAll, beforeAll, expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import type { Deps } from "../src/deps.ts";
import { createHandler } from "../src/server.ts";

let parent: string;
let handler: ReturnType<typeof createHandler>;
beforeAll(async () => {
  parent = await mkdtemp(join(tmpdir(), "chunk-dashboard-"));
  const dist = join(parent, "dist");
  await mkdir(join(dist, "assets"), { recursive: true });
  await writeFile(join(dist, "index.html"), "<!doctype html><title>chunk</title>");
  await writeFile(join(dist, "assets", "index-abc123.js"), "console.log('chunk')");
  await writeFile(join(parent, "secret.txt"), "operator secret");
  // Only the release store's routes are reached before the dashboard.
  const deps = { releases: { fetch: async () => undefined } } as unknown as Deps;
  handler = createHandler(deps, { dashboardDir: dist });
});
afterAll(() => rm(parent, { recursive: true, force: true }));

const get = (path: string, method = "GET") => handler(new Request(`http://chunk.test${path}`, { method }));

test("serves hashed assets as immutable and every other GET as index.html", async () => {
  const asset = await get("/assets/index-abc123.js");
  expect(asset.status).toBe(200);
  expect(asset.headers.get("content-type")).toStartWith("text/javascript");
  expect(asset.headers.get("cache-control")).toBe("public, max-age=31536000, immutable");
  expect(asset.headers.get("content-security-policy")).toBe("frame-ancestors 'none'");

  for (const path of ["/", "/login?code=ABCD-EFGH", "/p/prj_1/environments"]) {
    const page = await get(path);
    expect(await page.text()).toContain("<title>chunk</title>");
    expect(page.headers.get("content-type")).toStartWith("text/html");
    expect(page.headers.get("cache-control")).toBe("no-cache");
    expect(page.headers.get("content-security-policy")).toBe("frame-ancestors 'none'");
  }
  expect((await get("/assets/missing.js")).status).toBe(404);
  const head = await get("/login", "HEAD");
  expect(head.status).toBe(200);
  expect(await head.text()).toBe("");
  expect((await get("/", "POST")).status).toBe(404);
});

test("never serves files outside the dashboard directory", async () => {
  for (const path of ["/..%2fsecret.txt", "/assets/..%2f..%2fsecret.txt", "/%2e%2e/secret.txt", "/%00"]) {
    const response = await get(path);
    expect(await response.text()).not.toContain("operator secret");
  }
});

test("service routes come before the dashboard", async () => {
  expect(await (await get("/healthz")).text()).toBe("ok\n");
});

test("an install's routes answer before the dashboard, which serves the paths they pass on", async () => {
  const deps = { releases: { fetch: async () => undefined } } as unknown as Deps;
  const routed = createHandler(deps, {
    dashboardDir: join(parent, "dist"),
    routes: async (request) =>
      new URL(request.url).pathname === "/auth/start" ? Response.redirect("https://id.example.com/") : undefined,
  });
  const start = await routed(new Request("http://chunk.test/auth/start"));
  expect(start.headers.get("location")).toBe("https://id.example.com/");
  const page = await routed(new Request("http://chunk.test/auth/elsewhere"));
  expect(await page.text()).toContain("<title>chunk</title>");
});
