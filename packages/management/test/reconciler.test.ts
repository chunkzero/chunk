import { afterAll, beforeAll, describe, expect, test } from "bun:test";

import { Code } from "@connectrpc/connect";

import { capacityMachineName, coreMachineName, verifyJoinToken } from "../src/environments/machines.ts";
import { reconcile, type ReconcilerOptions } from "../src/environments/reconciler.ts";
import { CapacityState, EnvironmentService, Workload } from "../src/gen/chunk/management/v1/environment_pb.ts";
import { EnvironmentState, ProjectService } from "../src/gen/chunk/management/v1/projects_pb.ts";
import { fakeProvider } from "./fake-provider.ts";
import { codeOf, createEnvironment, databaseUrl, deployRelease, type Harness, next, startHarness } from "./harness.ts";

describe.skipIf(!databaseUrl)("reconciler", () => {
  let h: Harness;
  const { provider, machines, hooks } = fakeProvider();
  let options: ReconcilerOptions;
  beforeAll(async () => {
    h = await startHarness();
    options = { provider, image: "chunk/environment:test", managementUrl: h.url, coreMemoryMib: 2048, corePort: 7070 };
  });
  afterAll(() => h.close());

  const pass = () => reconcile(h.deps, options);

  /** A deployed environment whose core machine the reconciler created, attached as core. */
  async function running() {
    const { projectId, environmentId } = await createEnvironment(h);
    await deployRelease(h, projectId, environmentId, "r1");
    await pass();
    const coreName = coreMachineName(environmentId);
    const token = machines.get(coreName)?.spec.env.CHUNK_ENVIRONMENT_TOKEN ?? "";
    const client = h.client(EnvironmentService, token);
    const abort = new AbortController();
    const stream = client.attach({ instanceId: crypto.randomUUID(), core: true, epoch: 1n }, { signal: abort.signal });
    const { lease, revision } = await next(stream[Symbol.asyncIterator]());
    const state = async () => (await h.client(ProjectService).getEnvironment({ environmentId })).environment?.state;
    const core = () => machines.get(coreName)?.machine;
    return { environmentId, coreName, core, token, client, lease, revision, state, close: () => abort.abort() };
  }

  function capacityRequest(env: Awaited<ReturnType<typeof running>>, requestId: string) {
    return {
      requestId,
      workload: Workload.JVM,
      machineProfile: "default",
      releaseId: "r1",
      appId: "lobby",
      lease: env.lease,
    };
  }

  const nameOf = (environmentId: string, requestId: string) =>
    capacityMachineName({ environment_id: environmentId, request_id: requestId, workload: Workload.JVM } as never);

  test("core gets a machine with its own token, and the addresses it has once started", async () => {
    const env = await running();
    expect(env.core()?.state).toBe("running");
    expect(machines.get(env.coreName)?.spec).toMatchObject({ memoryMib: 2048, cpus: 1, restart: true });
    const [row] = await h.sql<{ machine_addresses: string[] }[]>`
      select machine_addresses from environments where id = ${env.environmentId}`;
    expect(row?.machine_addresses).toEqual(env.core()?.addresses ?? []);
    expect(row?.machine_addresses[1]).toMatch(/^10\.0\.0\./);
    expect(await env.state()).toBe(EnvironmentState.STARTING);
    env.close();
  });

  test("capacity requests are durable intents the reconciler provisions and removes", async () => {
    const env = await running();
    const request = capacityRequest(env, "cap-1");
    expect((await env.client.ensureCapacity(request)).capacity?.state).toBe(CapacityState.PROVISIONING);
    expect(await codeOf(env.client.ensureCapacity({ ...request, appId: "hub" }))).toBe(Code.AlreadyExists);
    expect(await codeOf(env.client.ensureCapacity({ ...request, requestId: "cap-2", machineProfile: "huge" }))).toBe(
      Code.InvalidArgument,
    );

    await pass();
    const ready = (await env.client.ensureCapacity(request)).capacity;
    expect(ready?.state).toBe(CapacityState.READY);
    const machine = machines.get(ready?.machineId ?? "");
    expect(machine?.spec).toMatchObject({ restart: false, env: { CHUNK_CORE_ADDRESS: `${env.coreName}:7070` } });
    expect(verifyJoinToken(env.token, machine?.spec.env.CHUNK_JOIN_TOKEN ?? "")).toMatchObject({
      environment_id: env.environmentId,
      request_id: "cap-1",
      workload: "jvm",
    });
    expect(verifyJoinToken("another token", machine?.spec.env.CHUNK_JOIN_TOKEN ?? "")).toBeUndefined();

    const released = await env.client.releaseCapacity({ requestId: "cap-1", lease: env.lease });
    expect(released.capacity?.state).toBe(CapacityState.RELEASED);
    await pass();
    expect(machines.has(ready?.machineId ?? "")).toBe(false);
    const unknown = await env.client.releaseCapacity({ requestId: "never", lease: env.lease });
    expect(unknown.capacity?.state).toBe(CapacityState.RELEASED);
    env.close();
  });

  test("an exited extra machine is replaced with a fresh join token", async () => {
    const env = await running();
    await env.client.ensureCapacity(capacityRequest(env, "cap-3"));
    await pass();
    const name = nameOf(env.environmentId, "cap-3");
    const first = machines.get(name)?.spec;
    await provider.stop(name);
    await pass();
    const replaced = machines.get(name);
    expect(replaced?.spec).not.toBe(first);
    expect(replaced?.machine.state).toBe("running");
    expect(verifyJoinToken(env.token, replaced?.spec.env.CHUNK_JOIN_TOKEN ?? "")?.request_id).toBe("cap-3");
    env.close();
  });

  test("a create whose reply was lost fails the request and its machine is removed by name", async () => {
    const env = await running();
    const name = nameOf(env.environmentId, "cap-4");
    hooks.create = (id) => {
      if (id === name) throw new Error("connection reset");
    };
    await env.client.ensureCapacity(capacityRequest(env, "cap-4"));
    await pass();
    hooks.create = undefined;
    expect(machines.has(name)).toBe(true);
    expect((await env.client.ensureCapacity(capacityRequest(env, "cap-4"))).capacity?.state).toBe(CapacityState.FAILED);
    await pass();
    expect(machines.has(name)).toBe(false);
    env.close();
  });

  test("an idle report suspends only for the current revision, retrying a failed suspension", async () => {
    const env = await running();
    const report = (sequence: bigint, desiredRevision: bigint) =>
      env.client.reportStatus({
        lease: env.lease,
        sequence,
        desiredRevision,
        gatewayAddresses: [`${env.coreName}:25565`],
        readyToSuspend: true,
      });

    await report(1n, env.revision - 1n);
    await pass();
    expect(await env.state()).toBe(EnvironmentState.RUNNING);

    await report(2n, env.revision);
    hooks.suspend = () => {
      throw new Error("engine unavailable");
    };
    await pass();
    hooks.suspend = undefined;
    expect(await env.state()).toBe(EnvironmentState.SUSPENDED);
    expect(env.core()?.state).toBe("running");
    await pass();
    expect(env.core()?.state).toBe("suspended");
    env.close();
  });

  test("a due alarm wakes a suspended environment, but not after a newer alarm replaced it", async () => {
    const env = await running();
    await env.client.reportStatus({
      lease: env.lease,
      sequence: 1n,
      desiredRevision: env.revision,
      readyToSuspend: true,
    });
    await pass();
    expect(await env.state()).toBe(EnvironmentState.SUSPENDED);

    const now = BigInt(Math.floor(Date.now() / 1000));
    const alarm = (generation: bigint, seconds: bigint) =>
      env.client.setWakeAlarm({ lease: env.lease, epoch: 1n, generation, dueTime: { seconds } });
    await alarm(1n, now - 1n);
    // The pass reads the due alarm, then a newer one replaces it before the pass fires it.
    hooks.status = async (id) => {
      if (id !== env.coreName) return;
      hooks.status = undefined;
      await alarm(2n, now + 3600n);
    };
    await pass();
    await pass();
    expect(await env.state()).toBe(EnvironmentState.SUSPENDED);

    await alarm(3n, now - 1n);
    await pass();
    await pass();
    expect(await env.state()).toBe(EnvironmentState.STARTING);
    expect(env.core()?.state).toBe("running");
    env.close();
  });

  test("deleting an environment whose core create reply was lost still removes the machine", async () => {
    const { projectId, environmentId } = await createEnvironment(h);
    await deployRelease(h, projectId, environmentId, "r1");
    const name = coreMachineName(environmentId);
    hooks.create = (id) => {
      if (id === name) throw new Error("connection reset");
    };
    await pass();
    hooks.create = undefined;
    expect(machines.has(name)).toBe(true);

    await h.client(ProjectService).deleteEnvironment({ environmentId });
    const state = h.client(ProjectService).getEnvironment({ environmentId });
    expect((await state).environment?.state).toBe(EnvironmentState.DELETING);
    await pass();
    expect(machines.has(name)).toBe(false);
    expect(await codeOf(h.client(ProjectService).getEnvironment({ environmentId }))).toBe(Code.NotFound);
  });

  test("a delete while core's token is being saved leaves no machine", async () => {
    const { projectId, environmentId } = await createEnvironment(h);
    await deployRelease(h, projectId, environmentId, "r1");
    const name = coreMachineName(environmentId);
    // A create whose reply is lost leaves a machine only cleanup by name would find.
    hooks.create = (id) => {
      if (id === name) throw new Error("connection reset");
    };
    let passing: Promise<void> | undefined;
    await h.sql.begin(async (tx) => {
      // This lock lets a token be issued for the environment but holds back saving it on the row.
      await tx`select 1 from environments where id = ${environmentId} for no key update`;
      passing = pass();
      // Wait until the pass is blocked on this row.
      for (let tries = 0; tries < 100; tries++) {
        const [waiting] = await h.sql<{ count: bigint }[]>`
          select count(*) from pg_stat_activity where wait_event_type = 'Lock' and datname = current_database()`;
        if ((waiting?.count ?? 0n) > 0n) break;
        await Bun.sleep(20);
      }
      await tx`delete from environments where id = ${environmentId}`;
    });
    await passing;
    hooks.create = undefined;
    await pass();
    expect(machines.has(name)).toBe(false);
    const [tokens] = await h.sql<{ count: bigint }[]>`
      select count(*) from api_tokens where environment_id = ${environmentId}`;
    expect(tokens?.count).toBe(0n);
  });

  test("a late activity report during an extra machine status lookup prevents both suspensions", async () => {
    const env = await running();
    try {
      await env.client.ensureCapacity(capacityRequest(env, "late-activity"));
      await pass();
      const extraName = nameOf(env.environmentId, "late-activity");
      const report = {
        lease: env.lease,
        desiredRevision: env.revision,
        gatewayAddresses: [`${env.coreName}:25565`],
      };
      await env.client.reportStatus({ ...report, sequence: 1n, readyToSuspend: true });

      // The extra machine's lookup happens after the conditional idle update has committed.
      hooks.status = async (id) => {
        if (id !== extraName) return;
        hooks.status = undefined;
        await env.client.reportStatus({ ...report, sequence: 2n, readyToSuspend: false, onlinePlayers: 1 });
      };
      await pass();

      const [activity] = await h.sql`
        select report_sequence, ready_to_suspend, online_players
        from environments where id = ${env.environmentId}`;
      expect(activity).toEqual({ report_sequence: 2n, ready_to_suspend: false, online_players: 1 });
      expect({ core: env.core()?.state, extra: machines.get(extraName)?.machine.state }).toEqual({
        core: "running",
        extra: "running",
      });
    } finally {
      hooks.status = undefined;
      env.close();
    }
  });

  test("a retried suspension is dropped when a newer report shows activity", async () => {
    const env = await running();
    const gateways = [`${env.coreName}:25565`];
    await env.client.reportStatus({
      lease: env.lease,
      sequence: 1n,
      desiredRevision: env.revision,
      gatewayAddresses: gateways,
      readyToSuspend: true,
    });
    hooks.suspend = () => {
      throw new Error("engine unavailable");
    };
    await pass();
    hooks.suspend = undefined;
    expect(await env.state()).toBe(EnvironmentState.SUSPENDED);

    // A player joins after the retry pass read the environment, before it suspends.
    hooks.status = async (id) => {
      if (id !== env.coreName) return;
      hooks.status = undefined;
      await env.client.reportStatus({
        lease: env.lease,
        sequence: 2n,
        desiredRevision: env.revision,
        gatewayAddresses: gateways,
        onlinePlayers: 1,
      });
    };
    await pass();
    expect(env.core()?.state).toBe("running");
    await pass();
    expect(await env.state()).toBe(EnvironmentState.STARTING);
    env.close();
  });

  test("deleting an environment revokes its token and removes its machines", async () => {
    const env = await running();
    env.close();
    await h.client(ProjectService).deleteEnvironment({ environmentId: env.environmentId });
    expect(await env.state()).toBe(EnvironmentState.DELETING);
    expect(await codeOf(env.client.reportUsage({}))).toBe(Code.Unauthenticated);
    await pass();
    expect(machines.has(env.coreName)).toBe(false);
    expect(await codeOf(env.state())).toBe(Code.NotFound);
  });
});
