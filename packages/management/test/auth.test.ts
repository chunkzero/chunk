import { afterAll, beforeAll, describe, expect, test } from "bun:test";

import { timestampFromDate } from "@bufbuild/protobuf/wkt";
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
