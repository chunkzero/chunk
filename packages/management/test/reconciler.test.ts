import { afterAll, beforeAll, describe, expect, test } from "bun:test";

import { Code } from "@connectrpc/connect";

import { verifyJoinToken } from "../src/environments/machines.ts";
import { reconcile, type ReconcilerOptions } from "../src/environments/reconciler.ts";
import { CapacityState, EnvironmentService, Workload } from "../src/gen/chunk/management/v1/environment_pb.ts";
import { EnvironmentState, ProjectService } from "../src/gen/chunk/management/v1/projects_pb.ts";
import { fakeProvider } from "./fake-provider.ts";
import { codeOf, createEnvironment, databaseUrl, deployRelease, type Harness, startHarness } from "./harness.ts";

describe.skipIf(!databaseUrl)("reconciler", () => {
  let h: Harness;
  const { provider, machines } = fakeProvider();
  let options: ReconcilerOptions;
  beforeAll(async () => {
    h = await startHarness();
    options = { provider, image: "chunk/environment:test", managementUrl: h.url, coreMemoryMib: 2048, corePort: 7070 };
  });
  afterAll(() => h.close());

  /** A deployed environment whose core machine the reconciler created, attached as core. */
  async function running() {
    const { projectId, environmentId } = await createEnvironment(h);
    await deployRelease(h, projectId, environmentId, "r1");
    await reconcile(h.deps, options);
    const core = machines.get(`chunk-${environmentId}-core`);
    const token = core?.spec.env.CHUNK_ENVIRONMENT_TOKEN ?? "";
    const client = h.client(EnvironmentService, token);
    const abort = new AbortController();
    const stream = client.attach({ instanceId: crypto.randomUUID(), core: true, epoch: 1n }, { signal: abort.signal });
    const messages = stream[Symbol.asyncIterator]();
    const { lease, revision } = (await messages.next()).value;
    const state = async () => (await h.client(ProjectService).getEnvironment({ environmentId })).environment?.state;
    return { environmentId, core, token, client, lease, revision, state, close: () => abort.abort() };
  }

  test("core gets a machine with its own token once something is deployed", async () => {
    const env = await running();
    expect(env.core?.machine.state).toBe("running");
    expect(env.core?.spec).toMatchObject({ memoryMib: 2048, cpus: 1, env: { CHUNK_MANAGEMENT_URL: h.url } });
    expect(await env.state()).toBe(EnvironmentState.STARTING);
    env.close();
  });

  test("capacity requests are durable intents the reconciler provisions and removes", async () => {
    const env = await running();
    const request = {
      requestId: "cap-1",
      workload: Workload.JVM,
      machineProfile: "default",
      releaseId: "r1",
      appId: "lobby",
      lease: env.lease,
    };
    expect((await env.client.ensureCapacity(request)).capacity?.state).toBe(CapacityState.PROVISIONING);
    expect(await codeOf(env.client.ensureCapacity({ ...request, appId: "hub" }))).toBe(Code.AlreadyExists);
    expect(await codeOf(env.client.ensureCapacity({ ...request, requestId: "cap-2", machineProfile: "huge" }))).toBe(
      Code.InvalidArgument,
    );

    await reconcile(h.deps, options);
    const ready = (await env.client.ensureCapacity({ ...request, lease: env.lease })).capacity;
    expect(ready?.state).toBe(CapacityState.READY);
    const machine = machines.get(ready?.machineId ?? "");
    expect(machine?.spec.env.CHUNK_CORE_ADDRESS).toBe(`${env.core?.machine.addresses[0]}:7070`);
    expect(verifyJoinToken(env.token, machine?.spec.env.CHUNK_JOIN_TOKEN ?? "")).toMatchObject({
      environment_id: env.environmentId,
      request_id: "cap-1",
      workload: "jvm",
    });
    expect(verifyJoinToken("another token", machine?.spec.env.CHUNK_JOIN_TOKEN ?? "")).toBeUndefined();

    const released = await env.client.releaseCapacity({ requestId: "cap-1", lease: env.lease });
    expect(released.capacity?.state).toBe(CapacityState.RELEASED);
    await reconcile(h.deps, options);
    expect(machines.has(ready?.machineId ?? "")).toBe(false);
    const unknown = await env.client.releaseCapacity({ requestId: "never", lease: env.lease });
    expect(unknown.capacity?.state).toBe(CapacityState.RELEASED);
    env.close();
  });

  test("an idle report suspends only for the current revision, and a due alarm wakes", async () => {
    const env = await running();
    const report = (sequence: bigint, desiredRevision: bigint) =>
      env.client.reportStatus({
        lease: env.lease,
        sequence,
        desiredRevision,
        gatewayAddresses: ["10.0.0.99:25565"],
        readyToSuspend: true,
      });

    await report(1n, env.revision - 1n);
    await reconcile(h.deps, options);
    expect(await env.state()).toBe(EnvironmentState.RUNNING);

    await report(2n, env.revision);
    await reconcile(h.deps, options);
    expect(await env.state()).toBe(EnvironmentState.SUSPENDED);
    expect(machines.get(env.core?.machine.id ?? "")?.machine.state).toBe("suspended");

    const now = BigInt(Math.floor(Date.now() / 1000));
    await env.client.setWakeAlarm({ lease: env.lease, epoch: 1n, generation: 1n, dueTime: { seconds: now - 1n } });
    await reconcile(h.deps, options);
    await reconcile(h.deps, options);
    expect(await env.state()).toBe(EnvironmentState.STARTING);
    expect(machines.get(env.core?.machine.id ?? "")?.machine.state).toBe("running");
    env.close();
  });

  test("deleting an environment revokes its token and removes its machines", async () => {
    const env = await running();
    env.close();
    await h.client(ProjectService).deleteEnvironment({ environmentId: env.environmentId });
    expect(await env.state()).toBe(EnvironmentState.DELETING);
    expect(await codeOf(env.client.reportUsage({}))).toBe(Code.Unauthenticated);
    await reconcile(h.deps, options);
    expect(machines.has(`chunk-${env.environmentId}-core`)).toBe(false);
    expect(await codeOf(env.state())).toBe(Code.NotFound);
  });
});
