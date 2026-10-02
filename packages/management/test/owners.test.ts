import { afterAll, beforeAll, describe, expect, test } from "bun:test";

import { Code } from "@connectrpc/connect";

import { tokenAuthenticator } from "../src/auth/tokens.ts";
import { AuthService } from "../src/gen/chunk/management/v1/auth_pb.ts";
import { DeploymentService } from "../src/gen/chunk/management/v1/deployments_pb.ts";
import { DomainService } from "../src/gen/chunk/management/v1/domains_pb.ts";
import { ProjectService } from "../src/gen/chunk/management/v1/projects_pb.ts";
import type { Identity, Owner } from "../src/rpc/caller.ts";
import { codeOf, databaseUrl, deployRelease, type Harness, startHarness } from "./harness.ts";

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
  ["nobody", person("nobody", [])],
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

  test("lookups by deployment, domain or token project hide other owners' projects", async () => {
    const projectId = (await createProject(h.operatorToken, "hidden", teamB.id)).project?.id ?? "";
    const { environment } = await h
      .client(ProjectService)
      .createEnvironment({ requestId: crypto.randomUUID(), projectId, name: "main" });
    const environmentId = environment?.id ?? "";
    const deploymentId = await deployRelease(h, projectId, environmentId, "hidden-1");
    const { domain } = await h.client(DomainService).addDomain({ environmentId, hostname: "play.hidden.example" });
    const domainId = domain?.id ?? "";

    // Alice reaches only another owner; nobody reaches none.
    for (const token of ["alice", "nobody"]) {
      expect(await codeOf(h.client(DeploymentService, token).getDeployment({ deploymentId }))).toBe(Code.NotFound);
      const domains = h.client(DomainService, token);
      expect(await codeOf(domains.verifyDomain({ domainId }))).toBe(Code.NotFound);
      await domains.removeDomain({ domainId });
      const created = h
        .client(AuthService, token)
        .createToken({ requestId: crypto.randomUUID(), name: "ci", projectId });
      expect(await codeOf(created)).toBe(Code.NotFound);
    }
    expect((await h.client(DomainService).listDomains({ environmentId })).domains.map((d) => d.id)).toEqual([domainId]);
  });

  test("a replay answers only while the caller still reaches the project's owner", async () => {
    people.set("carol", person("carol", [teamA]));
    const projects = h.client(ProjectService, "carol");
    const auth = h.client(AuthService, "carol");
    const projectRequest = { requestId: crypto.randomUUID(), name: "replayed", ownerId: "" };
    const project = (await projects.createProject(projectRequest)).project;
    const tokenRequest = { requestId: crypto.randomUUID(), name: "ci", projectId: project?.id ?? "" };
    const { secret } = await auth.createToken(tokenRequest);

    // A second owner leaves the one the project defaulted to reachable.
    people.set("carol", person("carol", [teamA, teamB]));
    expect((await projects.createProject(projectRequest)).project?.id).toBe(project?.id ?? "");
    expect((await auth.createToken(tokenRequest)).secret).toBe(secret);

    people.set("carol", person("carol", [teamB]));
    expect(await codeOf(projects.createProject(projectRequest))).toBe(Code.NotFound);
    expect(await codeOf(auth.createToken(tokenRequest))).toBe(Code.NotFound);
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
