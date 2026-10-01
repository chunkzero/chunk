import { afterAll, beforeAll, describe, expect, test } from "bun:test";

import { timestampDate, timestampFromDate } from "@bufbuild/protobuf/wkt";
import { Code } from "@connectrpc/connect";

import { AuthService, LoginState } from "../src/gen/chunk/management/v1/auth_pb.ts";
import { ProjectService } from "../src/gen/chunk/management/v1/projects_pb.ts";
import { codeOf, databaseUrl, type Harness, startHarness } from "./harness.ts";

describe.skipIf(!databaseUrl)("AuthService", () => {
  let h: Harness;
  beforeAll(async () => {
    h = await startHarness();
  });
  afterAll(() => h.close());

  test("rejects calls without a valid token", async () => {
    expect(await codeOf(h.client(AuthService, null).getCurrentPrincipal({}))).toBe(Code.Unauthenticated);
    expect(await codeOf(h.client(AuthService, "chunk_wrong").getCurrentPrincipal({}))).toBe(Code.Unauthenticated);
  });

  test("rejects unauthenticated oversized protected RPC bodies without reading them", async () => {
    const bytes = new TextEncoder().encode(`${" ".repeat(8 * 1024 * 1024)}{}`);
    let bytesRead = 0;
    const body = new ReadableStream<Uint8Array>(
      {
        pull(controller) {
          const chunk = bytes.subarray(bytesRead, bytesRead + 64 * 1024);
          bytesRead += chunk.byteLength;
          controller.enqueue(chunk);
          if (bytesRead === bytes.byteLength) controller.close();
        },
      },
      { highWaterMark: 0 },
    );
    const response = await h.fetch(
      new Request(`${h.url}/${AuthService.typeName}/GetCurrentPrincipal`, {
        method: "POST",
        headers: { "content-type": "application/json", "connect-protocol-version": "1" },
        body,
      }),
    );
    const error = (await response.json()) as { code?: string };
    expect(error.code).toBe("unauthenticated");
    expect(bytesRead, "unauthenticated RPC must be rejected before consuming its body").toBe(0);
  });

  test("rejects authenticated oversized protected RPC bodies by size", async () => {
    const response = await h.fetch(
      new Request(`${h.url}/${AuthService.typeName}/GetCurrentPrincipal`, {
        method: "POST",
        headers: {
          "content-type": "application/json",
          "connect-protocol-version": "1",
          authorization: `Bearer ${h.operatorToken}`,
        },
        body: `${" ".repeat(8 * 1024 * 1024)}{}`,
      }),
    );
    const error = (await response.json()) as { code?: string };
    expect(error.code).toBe("resource_exhausted");
  });

  test("rejects oversized public login RPC bodies by size", async () => {
    const response = await fetch(`${h.url}/${AuthService.typeName}/StartLogin`, {
      method: "POST",
      headers: { "content-type": "application/json", "connect-protocol-version": "1" },
      body: JSON.stringify({ clientName: "a".repeat(8 * 1024 * 1024) }),
    });
    if (response.status === 413) return;
    const error = (await response.json()) as { code?: string };
    expect(["resource_exhausted", "invalid_argument"]).toContain(error.code ?? `HTTP ${response.status}`);
  });

  test("device login issues a token once, after approval", async () => {
    const anonymous = h.client(AuthService, null);
    const login = await anonymous.startLogin({ clientName: "chunk CLI on laptop" });
    expect(login.userCode).toMatch(/^[A-Z]{4}-[A-Z]{4}$/);
    expect(login.verificationUrl).toBe(`${h.url}/login?code=${login.userCode}`);
    expect((await anonymous.pollLogin({ loginId: login.loginId })).state).toBe(LoginState.PENDING);

    await h.client(AuthService).approveLogin({ userCode: login.userCode.toLowerCase().replace("-", "") });
    const approved = await anonymous.pollLogin({ loginId: login.loginId });
    expect(approved.state).toBe(LoginState.APPROVED);
    expect(approved.secret).toStartWith("chunk_");
    const again = await anonymous.pollLogin({ loginId: login.loginId });
    expect(again.secret).toBe(approved.secret);
    expect(again.token?.id).toBe(approved.token?.id ?? "");
    await h.sql`update logins set expire_time = now() where user_code = ${login.userCode}`;
    expect(await codeOf(anonymous.pollLogin({ loginId: login.loginId }))).toBe(Code.NotFound);

    const current = await h.client(AuthService, approved.secret).getCurrentPrincipal({});
    expect(current.principal?.id).toBe("operator");
    expect(current.token?.name).toBe("chunk CLI on laptop");
    const [stored] = await h.sql`select secret_hash from api_tokens where id = ${current.token?.id ?? ""}`;
    expect(Buffer.from(stored?.secret_hash).toString()).not.toContain(approved.secret);
  });

  test("a device login token expires after 30 days unused and renews on use", async () => {
    const anonymous = h.client(AuthService, null);
    const login = await anonymous.startLogin({ clientName: "chunk CLI on desktop" });
    await h.client(AuthService).approveLogin({ userCode: login.userCode });
    const { secret } = await anonymous.pollLogin({ loginId: login.loginId });
    const client = h.client(AuthService, secret);
    const day = 24 * 60 * 60 * 1000;
    const tokenOf = async () => (await client.getCurrentPrincipal({})).token;
    const expiresInDays = async () => {
      const expireTime = (await tokenOf())?.expireTime;
      return ((expireTime ? timestampDate(expireTime).getTime() : 0) - Date.now()) / day;
    };
    expect(await expiresInDays()).toBeCloseTo(30, 0);
    const id = (await tokenOf())?.id ?? "";

    await h.sql`update api_tokens set expire_time = now() + interval '5 days' where id = ${id}`;
    await client.getCurrentPrincipal({});
    expect(await expiresInDays()).toBeCloseTo(30, 0);

    await h.sql`update api_tokens set expire_time = now() - interval '1 second' where id = ${id}`;
    expect(await codeOf(client.getCurrentPrincipal({}))).toBe(Code.Unauthenticated);
  });

  test("a token with a fixed expiry does not renew on use", async () => {
    const expireTime = new Date(Date.now() + 5 * 24 * 60 * 60 * 1000);
    const created = await h
      .client(AuthService)
      .createToken({ requestId: crypto.randomUUID(), name: "fixed", expireTime: timestampFromDate(expireTime) });
    const { token } = await h.client(AuthService, created.secret).getCurrentPrincipal({});
    expect(token?.expireTime && timestampDate(token.expireTime)).toEqual(expireTime);
  });

  test("tokens are idempotent by request_id and can be scoped and revoked", async () => {
    const auth = h.client(AuthService);
    const projects = h.client(ProjectService);
    const mine = await projects.createProject({ requestId: crypto.randomUUID(), name: "mine" });
    await projects.createProject({ requestId: crypto.randomUUID(), name: "theirs" });

    const request = { requestId: crypto.randomUUID(), name: "ci", projectId: mine.project?.id ?? "" };
    const created = await auth.createToken(request);
    const retried = await auth.createToken(request);
    expect(retried.secret).toBe(created.secret);
    expect(retried.token?.id).toBe(created.token?.id ?? "");
    expect(await codeOf(auth.createToken({ ...request, name: "other" }))).toBe(Code.AlreadyExists);
    // Another scope reusing the request_id makes an unrelated request rather than replaying this one.
    const scopedAuth = h.client(AuthService, created.secret);
    const unrelated = await scopedAuth.createToken(request);
    expect(unrelated.secret).not.toBe(created.secret);
    expect(await codeOf(scopedAuth.createToken({ ...request, requestId: crypto.randomUUID(), projectId: "" }))).toBe(
      Code.PermissionDenied,
    );

    const scoped = h.client(ProjectService, created.secret);
    const listed = await scoped.listProjects({});
    expect(listed.projects.map((project) => project.name)).toEqual(["mine"]);
    expect(await codeOf(scoped.createProject({ requestId: crypto.randomUUID(), name: "x" }))).toBe(
      Code.PermissionDenied,
    );
    expect(await codeOf(h.client(AuthService, created.secret).approveLogin({ userCode: "BBBB-BBBB" }))).toBe(
      Code.PermissionDenied,
    );

    expect((await auth.listTokens({})).tokens.map((token) => token.name)).toContain("ci");
    await auth.revokeToken({ tokenId: created.token?.id ?? "" });
    await auth.revokeToken({ tokenId: created.token?.id ?? "" });
    expect(await codeOf(scoped.listProjects({}))).toBe(Code.Unauthenticated);
  });

  test("a token retry replays its first result after expire_time passes", async () => {
    const auth = h.client(AuthService);
    const request = {
      requestId: crypto.randomUUID(),
      name: "short-lived",
      expireTime: timestampFromDate(new Date(Date.now() + 1000)),
    };
    const created = await auth.createToken(request);
    await Bun.sleep(1100);
    expect((await auth.createToken(request)).secret).toBe(created.secret);
    expect(await codeOf(auth.createToken({ ...request, requestId: crypto.randomUUID() }))).toBe(Code.InvalidArgument);
  });
});
