import { afterAll, beforeAll, describe, expect, test } from "bun:test";

import { Code } from "@connectrpc/connect";

import { notify } from "../src/changes.ts";
import { capacityCredentialContext, type CapacityRow } from "../src/environments/capacity.ts";
import {
  capacityMachineName,
  capacityMachineSpec,
  coreHostOf,
  coreMachineName,
  coreMachineSpec,
} from "../src/environments/machines.ts";
import {
  reconcile,
  type ReconcilerOptions,
  startReconciler,
  Superseded,
  takeLeadership,
} from "../src/environments/reconciler.ts";
import { CapacityState, EnvironmentService, Workload } from "../src/gen/chunk/management/v1/environment_pb.ts";
import { EnvironmentState, ProjectService } from "../src/gen/chunk/management/v1/projects_pb.ts";
import { fakeProvider } from "./fake-provider.ts";
import { releaseArchive } from "./fixtures.ts";
import {
  codeOf,
  createEnvironment,
  databaseUrl,
  deployRelease,
  type Harness,
  next,
  startHarness,
  uploadRelease,
} from "./harness.ts";

test("JVM machines run the runner for their Java and gateways the environment image, at core's first IP", () => {
  const options = {
    image: "chunk/environment:test",
    jvmImage: "ghcr.io/chunkzero/chunk-jvm:{java}",
    managementUrl: "",
    coreMemoryMib: 2048,
    corePort: 7070,
    trustedEdges: undefined,
  };
  const request = {
    environment_id: "env_1",
    request_id: "cap",
    workload: Workload.JVM,
    machine_profile: "default",
    release_id: "r1",
    app_id: "lobby",
    memory_mib: 512,
    java_version: 25,
  } as CapacityRow;
  expect(coreHostOf(["chunk-env-1-core", "10.0.0.2", "fdaa::1"])).toBe("10.0.0.2");
  expect(coreHostOf(["chunk-env-1-core"])).toBeUndefined();
  const spec = (request: CapacityRow, coreHost = coreHostOf(["chunk-env-1-core", "fdaa::1"]) ?? "") =>
    capacityMachineSpec(options, request, { coreHost, credential: "secret" });

  expect(spec(request)).toMatchObject({
    image: "ghcr.io/chunkzero/chunk-jvm:25",
    env: {
      CHUNK_CORE_ENDPOINT: "http://[fdaa::1]:7070",
      CHUNK_JVM_CREDENTIAL: "secret",
      CHUNK_ENVIRONMENT_ID: "env_1",
      CHUNK_RELEASE_ID: "r1",
      CHUNK_APP_ID: "lobby",
      CHUNK_MACHINE_PROFILE: "default",
    },
  });
  expect(Object.keys(spec(request).env)).toHaveLength(6);
  expect(() => spec({ ...request, java_version: null })).toThrow("no JVM image");
  expect(() =>
    capacityMachineSpec({ ...options, jvmImage: undefined }, request, { coreHost: "10.0.0.2", credential: "" }),
  ).toThrow("no JVM image");

  const gateway = spec({ ...request, workload: Workload.GATEWAY, app_id: "", java_version: null }, "10.0.0.2");
  expect(gateway.image).toBe("chunk/environment:test");
  expect(gateway.env).toMatchObject({
    CHUNK_SERVICES: "gateway",
    CHUNK_CORE_ENDPOINT: "http://10.0.0.2:7070",
    CHUNK_GATEWAY_CREDENTIAL: "secret",
  });
});

test("core and gateway machines trust the configured edges, and JVM machines never see them", () => {
  const options = {
    image: "chunk/environment:test",
    jvmImage: "chunk-jvm:{java}",
    managementUrl: "",
    coreMemoryMib: 1024,
    corePort: 7070,
    trustedEdges: "10.231.0.2",
  };
  const request = { environment_id: "env_1", request_id: "cap", memory_mib: 512, java_version: 25 } as CapacityRow;
  const spec = (workload: Workload) =>
    capacityMachineSpec(options, { ...request, workload }, { coreHost: "10.0.0.2", credential: "secret" });

  expect(coreMachineSpec(options, "env_1", "token").env.CHUNK_TRUSTED_EDGES).toBe("10.231.0.2");
  expect(spec(Workload.GATEWAY).env.CHUNK_TRUSTED_EDGES).toBe("10.231.0.2");
  expect(spec(Workload.JVM).env).not.toHaveProperty("CHUNK_TRUSTED_EDGES");
  expect(coreMachineSpec({ ...options, trustedEdges: undefined }, "env_1", "token").env).not.toHaveProperty(
    "CHUNK_TRUSTED_EDGES",
  );
});

describe.skipIf(!databaseUrl)("reconciler", () => {
  let h: Harness;
  const { provider, machines, volumes, boots, hooks } = fakeProvider();
  let options: ReconcilerOptions;
  let epoch = 0n;
  beforeAll(async () => {
    h = await startHarness();
    epoch = await takeLeadership(h.sql);
    options = {
      provider,
      image: "chunk/environment:test",
      jvmImage: "chunk-jvm:{java}",
      managementUrl: h.url,
      coreMemoryMib: 2048,
      corePort: 7070,
      trustedEdges: undefined,
    };
  });
  afterAll(() => h.close());

  const pass = () => reconcile(h.deps, options, epoch);

  /** A deployed environment whose core machine the reconciler created, attached as core. */
  async function running() {
    const { projectId, environmentId } = await createEnvironment(h);
    await deployRelease(h, projectId, environmentId, "r1");
    await pass();
    const coreName = coreMachineName(environmentId);
    const token = machines.get(coreName)?.spec.env.CHUNK_ENVIRONMENT_TOKEN ?? "";
    const client = h.client(EnvironmentService, token);
    const abort = new AbortController();
    const instanceId = crypto.randomUUID();
    const stream = client.attach({ instanceId, core: true, epoch: 1n }, { signal: abort.signal });
    const { lease, revision } = await next(stream[Symbol.asyncIterator]());
    const state = async () => (await h.client(ProjectService).getEnvironment({ environmentId })).environment?.state;
    const core = () => machines.get(coreName)?.machine;
    return {
      projectId,
      environmentId,
      coreName,
      core,
      token,
      client,
      instanceId,
      lease,
      revision,
      state,
      close: () => abort.abort(),
    };
  }

  function capacityRequest(env: Awaited<ReturnType<typeof running>>, requestId: string) {
    return {
      requestId,
      workload: Workload.JVM,
      machineProfile: "default",
      releaseId: "r1",
      appId: "lobby",
      lease: env.lease,
      credential: `machine/v1/${env.environmentId}/jvm/${requestId}/secret`,
    };
  }

  const nameOf = (environmentId: string, requestId: string, workload = Workload.JVM) =>
    capacityMachineName({ environment_id: environmentId, request_id: requestId, workload } as never);
  const idOf = (name: string) => machines.get(name)?.machine.id ?? "";

  /** Waits until some query is blocked on a lock; false when none was within two seconds. */
  async function lockWaited() {
    for (let tries = 0; tries < 100; tries++) {
      const [waiting] = await h.sql<{ count: bigint }[]>`
        select count(*) from pg_stat_activity where wait_event_type = 'Lock' and datname = current_database()`;
      if ((waiting?.count ?? 0n) > 0n) return true;
      await Bun.sleep(20);
    }
    return false;
  }

  async function until(condition: () => boolean) {
    for (let tries = 0; tries < 200 && !condition(); tries++) await Bun.sleep(50);
  }

  /** Runs a pass under the current epoch that `takeOver` supersedes, expecting it to stop at its next write. */
  async function supersededPass() {
    const stale = epoch;
    await expect(reconcile(h.deps, options, stale)).rejects.toThrow(Superseded);
  }
  async function takeOver() {
    epoch = await takeLeadership(h.sql);
    await pass();
  }

  test("core gets a machine with its own token, and the addresses it has once started", async () => {
    const env = await running();
    expect(env.core()?.state).toBe("running");
    expect(machines.get(env.coreName)?.spec).toMatchObject({
      memoryMib: 2048,
      cpus: 1,
      restart: true,
      env: { CHUNK_SERVICES: "core,gateway", CHUNK_CORE_BIND: "[::]:7070" },
    });
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
    expect((await env.client.ensureCapacity(request)).capacity?.state).toBe(CapacityState.PROVISIONING);
    expect(await codeOf(env.client.ensureCapacity({ ...request, appId: "hub" }))).toBe(Code.AlreadyExists);
    expect(await codeOf(env.client.ensureCapacity({ ...request, credential: "another" }))).toBe(Code.AlreadyExists);
    expect(await codeOf(env.client.ensureCapacity({ ...request, requestId: "cap-2", credential: "" }))).toBe(
      Code.InvalidArgument,
    );
    expect(await codeOf(env.client.ensureCapacity({ ...request, requestId: "cap-2", machineProfile: "huge" }))).toBe(
      Code.InvalidArgument,
    );
    // 3 is the reserved WORKLOAD_EXEC; without an app ID, only the workload check can refuse it.
    expect(
      await codeOf(env.client.ensureCapacity({ ...request, requestId: "cap-2", workload: 3 as Workload, appId: "" })),
    ).toBe(Code.InvalidArgument);
    await uploadRelease(
      h,
      env.projectId,
      "r-java",
      releaseArchive("r-java", undefined, { manifest: (m) => ({ ...m, java_version: 25.5 }) }),
    );
    expect(await codeOf(env.client.ensureCapacity({ ...request, requestId: "cap-2", releaseId: "r-java" }))).toBe(
      Code.FailedPrecondition,
    );

    // Extra machines wait while core has no IP address.
    const core = machines.get(env.coreName);
    const coreIp = core?.machine.addresses[1] ?? "";
    if (core) core.machine = { ...core.machine, addresses: [env.coreName] };
    await pass();
    expect(machines.has(nameOf(env.environmentId, "cap-1"))).toBe(false);
    if (core) core.machine = { ...core.machine, addresses: [env.coreName, coreIp] };

    await pass();
    const ready = (await env.client.ensureCapacity(request)).capacity;
    expect(ready?.state).toBe(CapacityState.READY);
    const name = nameOf(env.environmentId, "cap-1");
    expect(ready?.machineId).toBe(idOf(name));
    const machine = machines.get(name);
    expect(machine?.spec).toMatchObject({
      image: "chunk-jvm:25",
      restart: false,
      env: { CHUNK_CORE_ENDPOINT: `http://${coreIp}:7070`, CHUNK_JVM_CREDENTIAL: request.credential },
    });

    const [stored] = await h.sql<{ credential: Uint8Array }[]>`
      select credential from capacity_requests where environment_id = ${env.environmentId} and request_id = 'cap-1'`;
    const sealed = Buffer.from(stored?.credential ?? []);
    expect(sealed.includes(request.credential)).toBe(false);
    const open = (requestId: string) =>
      h.deps.keys.cipher.open(sealed, capacityCredentialContext(env.environmentId, requestId));
    expect(new TextDecoder().decode(await open("cap-1"))).toBe(request.credential);
    await expect(open("cap-2")).rejects.toThrow();

    const released = await env.client.releaseCapacity({ requestId: "cap-1", lease: env.lease });
    expect(released.capacity?.state).toBe(CapacityState.RELEASING);
    await pass();
    expect(machines.has(name)).toBe(false);
    expect((await env.client.ensureCapacity(request)).capacity?.state).toBe(CapacityState.RELEASED);

    // An ensure delayed past the release of its ID finds it released.
    const unknown = await env.client.releaseCapacity({ requestId: "never", lease: env.lease });
    expect(unknown.capacity?.state).toBe(CapacityState.RELEASED);
    const late = { ...capacityRequest(env, "never"), credential: "late" };
    expect((await env.client.ensureCapacity(late)).capacity?.state).toBe(CapacityState.RELEASED);
    await pass();
    expect(machines.has(name)).toBe(false);
    expect(machines.has(nameOf(env.environmentId, "never"))).toBe(false);
    env.close();
  });

  test("an exited gateway machine is replaced with the same credential", async () => {
    const env = await running();
    await env.client.ensureCapacity({ ...capacityRequest(env, "cap-3"), workload: Workload.GATEWAY, appId: "" });
    await pass();
    const name = nameOf(env.environmentId, "cap-3", Workload.GATEWAY);
    const first = machines.get(name)?.spec;
    expect(first?.env.CHUNK_GATEWAY_CREDENTIAL).toBe(capacityRequest(env, "cap-3").credential);
    await provider.stop(idOf(name));
    await pass();
    const replaced = machines.get(name);
    expect(replaced?.spec).not.toBe(first);
    expect(replaced?.machine.state).toBe("running");
    expect(replaced?.spec.env.CHUNK_GATEWAY_CREDENTIAL).toBe(first?.env.CHUNK_GATEWAY_CREDENTIAL);
    env.close();
  });

  test("an exited JVM machine fails its request, and release waits for its removal", async () => {
    const env = await running();
    const request = capacityRequest(env, "cap-jvm");
    await env.client.ensureCapacity(request);
    await pass();
    const name = nameOf(env.environmentId, "cap-jvm");
    const first = machines.get(name)?.spec;
    await provider.stop(idOf(name));
    await pass();
    expect((await env.client.ensureCapacity(request)).capacity).toMatchObject({
      state: CapacityState.FAILED,
      message: "the JVM machine exited",
    });
    expect(machines.get(name)?.spec).toBe(first);
    expect(machines.get(name)?.machine.state).toBe("stopped");

    const release = () => env.client.releaseCapacity({ requestId: "cap-jvm", lease: env.lease });
    expect((await release()).capacity?.state).toBe(CapacityState.RELEASING);
    expect((await release()).capacity?.state).toBe(CapacityState.RELEASING);
    await pass();
    expect(machines.has(name)).toBe(false);
    expect((await release()).capacity?.state).toBe(CapacityState.RELEASED);
    env.close();
  });

  test("a release racing a pass's start leaves no machine once RELEASED", async () => {
    const env = await running();
    const release = (requestId: string) => env.client.releaseCapacity({ requestId, lease: env.lease });
    // Released once the pass read the suspended machine, before it resumes it.
    await env.client.ensureCapacity(capacityRequest(env, "suspended"));
    await pass();
    const suspended = nameOf(env.environmentId, "suspended");
    await provider.suspend(idOf(suspended));
    hooks.status = async (id) => {
      if (id === suspended) await release("suspended");
    };
    // Released once the pass created the machine, before it saved and started it.
    await env.client.ensureCapacity(capacityRequest(env, "created"));
    const created = nameOf(env.environmentId, "created");
    hooks.create = async (id) => {
      if (id === created) await release("created");
    };
    // Released once the pass recorded the boot, while the start is under way.
    await env.client.ensureCapacity(capacityRequest(env, "starting"));
    const starting = nameOf(env.environmentId, "starting");
    hooks.start = async (name) => {
      if (name === starting) expect((await release("starting")).capacity?.state).toBe(CapacityState.RELEASING);
    };
    try {
      await pass();
    } finally {
      hooks.status = undefined;
      hooks.create = undefined;
      hooks.start = undefined;
    }
    expect(machines.get(suspended)?.machine.state).toBe("suspended");
    expect(machines.has(created)).toBe(false);
    await pass();
    for (const requestId of ["suspended", "created", "starting"]) {
      expect(machines.has(nameOf(env.environmentId, requestId))).toBe(false);
      expect((await release(requestId)).capacity?.state).toBe(CapacityState.RELEASED);
    }
    env.close();
  });

  test("a JVM machine created before a crash is adopted and boots once", async () => {
    const env = await running();
    const request = capacityRequest(env, "cap-crash");
    await env.client.ensureCapacity(request);
    // What a pass that crashed between creating the machine and saving its ID leaves behind.
    const [row] = await h.sql<CapacityRow[]>`
      select * from capacity_requests where environment_id = ${env.environmentId} and request_id = 'cap-crash'`;
    const coreHost = coreHostOf(env.core()?.addresses ?? []) ?? "";
    await provider.create(
      capacityMachineSpec(options, row as CapacityRow, { coreHost, credential: request.credential }),
    );
    const name = nameOf(env.environmentId, "cap-crash");
    const created = machines.get(name)?.spec;
    let boots = 0;
    hooks.start = (id) => {
      if (id === name) boots++;
    };
    try {
      await pass();
      await pass();
    } finally {
      hooks.start = undefined;
    }
    expect(machines.get(name)?.spec).toBe(created);
    expect(boots).toBe(1);
    expect((await env.client.ensureCapacity(request)).capacity).toMatchObject({
      state: CapacityState.READY,
      machineId: idOf(name),
    });
    env.close();
  });

  test("a new core instance releases the previous one's requests, but a re-attach does not", async () => {
    const env = await running();
    await env.client.ensureCapacity(capacityRequest(env, "cap-owned"));
    await pass();
    const name = nameOf(env.environmentId, "cap-owned");
    const attach = async (instanceId: string) => {
      const abort = new AbortController();
      const stream = env.client.attach({ instanceId, core: true, epoch: 1n }, { signal: abort.signal });
      await next(stream[Symbol.asyncIterator]());
      abort.abort();
    };
    const state = async () => {
      const [row] = await h.sql<{ state: CapacityState }[]>`
        select state from capacity_requests where environment_id = ${env.environmentId} and request_id = 'cap-owned'`;
      return row?.state;
    };

    await attach(env.instanceId);
    await pass();
    expect(await state()).toBe(CapacityState.READY);
    expect(machines.get(name)?.machine.state).toBe("running");

    await attach(crypto.randomUUID());
    expect(await state()).toBe(CapacityState.RELEASING);
    await pass();
    expect(await state()).toBe(CapacityState.RELEASED);
    expect(machines.has(name)).toBe(false);
    env.close();
  });

  test("only the reconciler holding the leader lock acts, and another takes over once it stops", async () => {
    const other = await startHarness();
    const fakes = [fakeProvider(), fakeProvider()];
    const reconcilers = fakes.map(({ provider }) =>
      startReconciler(other.deps, { ...options, provider, managementUrl: other.url }, databaseUrl ?? ""),
    );
    try {
      const { projectId, environmentId } = await createEnvironment(other);
      await deployRelease(other, projectId, environmentId, "r1");
      const name = coreMachineName(environmentId);
      const acted = () => fakes.map(({ machines }) => machines.has(name));
      await until(() => acted().some(Boolean));
      await Bun.sleep(100);
      expect(acted().filter(Boolean)).toHaveLength(1);

      await reconcilers[acted().indexOf(true)]?.stop();
      await notify(other.sql, { kind: "environment", environmentId });
      await until(() => acted().every(Boolean));
      expect(acted()).toEqual([true, true]);
    } finally {
      await Promise.all(reconcilers.map((reconciler) => reconciler.stop()));
      await other.close();
    }
  });

  test("a leader superseded mid-pass saves and starts nothing, so its JVM machine boots once", async () => {
    const env = await running();
    const request = capacityRequest(env, "cap-stale");
    await env.client.ensureCapacity(request);
    const name = nameOf(env.environmentId, "cap-stale");
    let boots = 0;
    hooks.start = (id) => {
      if (id === name) boots++;
    };
    // Once the stale pass created the machine, a new leader adopts and starts it, and it exits.
    hooks.create = async (id) => {
      if (id !== name) return;
      hooks.create = undefined;
      await takeOver();
      await provider.stop(idOf(name));
    };
    try {
      await supersededPass();
      await pass();
    } finally {
      hooks.start = undefined;
      hooks.create = undefined;
    }
    expect(boots).toBe(1);
    expect((await env.client.ensureCapacity(request)).capacity).toMatchObject({
      state: CapacityState.FAILED,
      message: "the JVM machine exited",
    });
    env.close();
  });

  test("a JVM boot recorded before its leader was superseded is the only one", async () => {
    const env = await running();
    const request = capacityRequest(env, "boot-once");
    await env.client.ensureCapacity(request);
    const name = nameOf(env.environmentId, "boot-once");
    // The boot is committed and the start under way when a new leader takes over and finds the machine not running.
    hooks.start = async (started) => {
      if (started !== name) return;
      hooks.start = undefined;
      await takeOver();
    };
    try {
      await supersededPass();
    } finally {
      hooks.start = undefined;
    }
    expect(boots.get(name)).toBe(1);
    expect((await env.client.ensureCapacity(request)).capacity).toMatchObject({
      state: CapacityState.FAILED,
      message: "the JVM machine exited",
    });
    await pass();
    expect(boots.get(name)).toBe(1);
    expect(machines.has(name)).toBe(false);
    env.close();
  });

  test("a start under way when its released machine is torn down boots nothing", async () => {
    const env = await running();
    await env.client.ensureCapacity(capacityRequest(env, "start-late"));
    const name = nameOf(env.environmentId, "start-late");
    hooks.start = async (started) => {
      if (started !== name) return;
      hooks.start = undefined;
      await env.client.releaseCapacity({ requestId: "start-late", lease: env.lease });
      await takeOver();
    };
    try {
      await supersededPass();
    } finally {
      hooks.start = undefined;
    }
    expect(machines.has(name)).toBe(false);
    expect(boots.get(name)).toBeUndefined();
    const released = await env.client.releaseCapacity({ requestId: "start-late", lease: env.lease });
    expect(released.capacity?.state).toBe(CapacityState.RELEASED);
    env.close();
  });

  test("a superseded pass replacing a stopped gateway leaves the new leader's replacement running", async () => {
    const env = await running();
    const request = { ...capacityRequest(env, "gateway-stale"), workload: Workload.GATEWAY, appId: "" };
    await env.client.ensureCapacity(request);
    await pass();
    const name = nameOf(env.environmentId, "gateway-stale", Workload.GATEWAY);
    const first = idOf(name);
    await provider.stop(first);
    // The new leader replaces the stopped gateway while the stale pass's removal of it is under way.
    hooks.destroy = async (destroyed) => {
      if (destroyed !== name) return;
      hooks.destroy = undefined;
      await takeOver();
    };
    try {
      await supersededPass();
    } finally {
      hooks.destroy = undefined;
    }
    const replacement = machines.get(name)?.machine;
    expect(replacement?.id).not.toBe(first);
    expect(replacement?.state).toBe("running");
    expect((await env.client.ensureCapacity(request)).capacity).toMatchObject({
      state: CapacityState.READY,
      machineId: replacement?.id,
    });
    env.close();
  });

  test("a core volume left by a create cut short after its environment was deleted is swept", async () => {
    const { environmentId } = await createEnvironment(h);
    await h.client(ProjectService).deleteEnvironment({ environmentId });
    await pass();
    const name = coreMachineName(environmentId);
    // A stale create resumes, creates the volume, and crashes before the machine exists.
    hooks.creating = () => {
      throw new Error("crashed");
    };
    try {
      await expect(provider.create(coreMachineSpec(options, environmentId, "token"))).rejects.toThrow("crashed");
    } finally {
      hooks.creating = undefined;
    }
    expect(volumes.has(name)).toBe(true);
    expect(machines.has(name)).toBe(false);
    await pass();
    expect(volumes.has(name)).toBe(false);
  });

  test("a create that finishes after its request was released never starts, and a later pass sweeps it", async () => {
    const env = await running();
    const release = (requestId: string) => env.client.releaseCapacity({ requestId, lease: env.lease });
    const started: string[] = [];
    hooks.start = (id) => {
      started.push(id);
    };
    /** Releases the request and tears it down while its machine's create is under way. */
    const releasedDuringCreate = async (requestId: string, newLeader: boolean) => {
      await env.client.ensureCapacity(capacityRequest(env, requestId));
      const name = nameOf(env.environmentId, requestId);
      hooks.creating = async (id) => {
        if (id !== name) return;
        hooks.creating = undefined;
        await release(requestId);
        if (newLeader) epoch = await takeLeadership(h.sql);
        await pass();
      };
      return name;
    };
    try {
      // Under a new leader, and the stale create's reply is lost.
      const lost = await releasedDuringCreate("late-lost", true);
      hooks.create = (id) => {
        if (id === lost) throw new Error("connection reset");
      };
      const stale = epoch;
      await expect(reconcile(h.deps, options, stale)).rejects.toThrow(Superseded);
      expect(machines.get(lost)?.machine.state).toBe("stopped");
      expect((await release("late-lost")).capacity?.state).toBe(CapacityState.RELEASED);

      // Under the same leader, whose destroy of the machine it could not save fails.
      const unsaved = await releasedDuringCreate("late-unsaved", false);
      hooks.destroy = (name) => {
        if (name === unsaved && machines.has(name)) throw new Error("engine unavailable");
      };
      await pass();
      expect(machines.has(lost)).toBe(false);
      expect(machines.get(unsaved)?.machine.state).toBe("stopped");
      expect((await release("late-unsaved")).capacity?.state).toBe(CapacityState.RELEASED);
      hooks.destroy = undefined;
      await pass();
      expect(machines.has(unsaved)).toBe(false);
      expect(started.filter((id) => id === lost || id === unsaved)).toEqual([]);
    } finally {
      hooks.start = undefined;
      hooks.creating = undefined;
      hooks.create = undefined;
      hooks.destroy = undefined;
    }
    env.close();
  });

  test("a core create that finishes after its environment was deleted is swept", async () => {
    const { projectId, environmentId } = await createEnvironment(h);
    await deployRelease(h, projectId, environmentId, "r1");
    const name = coreMachineName(environmentId);
    hooks.creating = async (id) => {
      if (id !== name) return;
      hooks.creating = undefined;
      await h.client(ProjectService).deleteEnvironment({ environmentId });
      epoch = await takeLeadership(h.sql);
      await pass();
    };
    const stale = epoch;
    try {
      await expect(reconcile(h.deps, options, stale)).rejects.toThrow(Superseded);
    } finally {
      hooks.creating = undefined;
    }
    expect(machines.has(name)).toBe(true);
    expect(await codeOf(h.client(ProjectService).getEnvironment({ environmentId }))).toBe(Code.NotFound);
    await pass();
    expect(machines.has(name)).toBe(false);
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
      await lockWaited();
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
