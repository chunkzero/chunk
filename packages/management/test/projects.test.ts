import { afterAll, beforeAll, describe, expect, test } from "bun:test";

import { Code } from "@connectrpc/connect";

import { SleepingPingMode } from "../src/gen/chunk/management/v1/common_pb.ts";
import { EnvironmentState, ProjectService } from "../src/gen/chunk/management/v1/projects_pb.ts";
import { codeOf, databaseUrl, type Harness, startHarness } from "./harness.ts";

describe.skipIf(!databaseUrl)("ProjectService", () => {
  let h: Harness;
  beforeAll(async () => {
    h = await startHarness();
  });
  afterAll(() => h.close());

  test("creates projects and environments idempotently with unique names", async () => {
    const projects = h.client(ProjectService);
    const request = { requestId: crypto.randomUUID(), name: "survival" };
    const project = (await projects.createProject(request)).project;
    expect((await projects.createProject(request)).project?.id).toBe(project?.id ?? "");
    expect(await codeOf(projects.createProject({ ...request, requestId: crypto.randomUUID() }))).toBe(
      Code.AlreadyExists,
    );
    expect(await codeOf(projects.createProject({ requestId: crypto.randomUUID(), name: "Bad Name" }))).toBe(
      Code.InvalidArgument,
    );

    const projectId = project?.id ?? "";
    const names = ["production", "staging", "preview"];
    for (const name of names) await projects.createEnvironment({ requestId: crypto.randomUUID(), projectId, name });
    expect(
      await codeOf(projects.createEnvironment({ requestId: crypto.randomUUID(), projectId, name: "staging" })),
    ).toBe(Code.AlreadyExists);

    const seen: string[] = [];
    let pageToken = "";
    do {
      const page = await projects.listEnvironments({ projectId, pageSize: 2, pageToken });
      seen.push(...page.environments.map((environment) => environment.name));
      pageToken = page.nextPageToken;
    } while (pageToken);
    expect(seen).toEqual(names);

    const [production] = (await projects.listEnvironments({ projectId })).environments;
    expect(production?.state).toBe(EnvironmentState.PENDING);
    expect(production?.sleepingPing).toBe(SleepingPingMode.CACHE);
    expect(production?.hostname).toBe(`${production?.id.replace("_", "-")}.play.example.net`);
    const updated = await projects.updateEnvironment({
      environmentId: production?.id ?? "",
      sleepingPing: SleepingPingMode.WAKE,
    });
    expect(updated.environment?.sleepingPing).toBe(SleepingPingMode.WAKE);
    expect((await projects.listSnapshots({ environmentId: production?.id ?? "" })).snapshots).toEqual([]);

    await projects.deleteEnvironment({ environmentId: production?.id ?? "" });
    await projects.deleteEnvironment({ environmentId: production?.id ?? "" });
    expect(await codeOf(projects.getEnvironment({ environmentId: production?.id ?? "" }))).toBe(Code.NotFound);
  });

  test("forks are not implemented yet", async () => {
    expect(
      await codeOf(h.client(ProjectService).forkEnvironment({ requestId: crypto.randomUUID(), name: "fork" })),
    ).toBe(Code.Unimplemented);
  });
});
