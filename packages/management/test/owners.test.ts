import { afterAll, beforeAll, describe, expect, test } from "bun:test";

import { Code } from "@connectrpc/connect";

import { tokenAuthenticator } from "../src/auth/tokens.ts";
import { AuthService } from "../src/gen/chunk/management/v1/auth_pb.ts";
import { ProjectService } from "../src/gen/chunk/management/v1/projects_pb.ts";
import type { Identity, Owner } from "../src/rpc/caller.ts";
import { codeOf, databaseUrl, type Harness, startHarness } from "./harness.ts";

const teamA: Owner = { id: "team-a", displayName: "Team A" };
const teamB: Owner = { id: "team-b", displayName: "Team B" };

const person = (id: string, owners: Owner[]): Identity => ({
  kind: "person",
  caller: { principal: { id, displayName: id }, tokenId: "", projectId: undefined, owners },
});

/** People an install's authenticator resolves, by bearer, each limited to some owners. */
const people = new Map([
  ["alice", person("alice", [teamA])],
  ["bob", person("bob", [teamA, teamB])],
]);

describe.skipIf(!databaseUrl)("owners", () => {
  let h: Harness;
  beforeAll(async () => {
    h = await startHarness(
      {},
      {
        authenticator: {
          authenticate: async (bearer) => people.get(bearer) ?? tokenAuthenticator(h.db).authenticate(bearer),
        },
        signInOptions: [{ label: "Continue with Example", url: "/auth/example/start" }],
      },
    );
  });
  afterAll(() => h.close());

  const createProject = (token: string, name: string, ownerId = "") =>
    h.client(ProjectService, token).createProject({ requestId: crypto.randomUUID(), name, ownerId });

  test("a caller reaches only its owners' projects, and others' answer NOT_FOUND", async () => {
    const mine = (await createProject(h.operatorToken, "ours", teamA.id)).project;
    const theirs = (await createProject(h.operatorToken, "theirs", teamB.id)).project;
    const operator = h.client(ProjectService);
    const { environment } = await operator.createEnvironment({
      requestId: crypto.randomUUID(),
      projectId: theirs?.id ?? "",
      name: "main",
    });
    const environmentId = environment?.id ?? "";

    const alice = h.client(ProjectService, "alice");
    expect((await alice.listProjects({})).projects.map((project) => project.id)).toEqual([mine?.id ?? ""]);
    expect(await codeOf(alice.getProject({ projectId: theirs?.id ?? "" }))).toBe(Code.NotFound);
    expect(await codeOf(alice.getEnvironment({ environmentId }))).toBe(Code.NotFound);
    // Deleting answers as for a missing environment, and leaves it alone.
    await alice.deleteEnvironment({ environmentId });
    expect((await operator.getEnvironment({ environmentId })).environment?.id).toBe(environmentId);

    const principal = await h.client(AuthService, "alice").getCurrentPrincipal({});
    expect(principal.owners.map(({ id, displayName }) => ({ id, displayName }))).toEqual([teamA]);
  });

  test("a new project defaults to the caller's only owner and must be one of its owners", async () => {
    expect((await createProject("alice", "defaulted")).project?.ownerId).toBe(teamA.id);
    expect(await codeOf(createProject("alice", "foreign", teamB.id))).toBe(Code.PermissionDenied);
    expect(await codeOf(createProject("bob", "ambiguous"))).toBe(Code.InvalidArgument);
    expect((await createProject("bob", "chosen", teamB.id)).project?.ownerId).toBe(teamB.id);
  });

  test("sign-in options need no token", async () => {
    const { options } = await h.client(AuthService, null).getSignInOptions({});
    expect(options.map(({ label, url }) => ({ label, url }))).toEqual([
      { label: "Continue with Example", url: "/auth/example/start" },
    ]);
  });
});
