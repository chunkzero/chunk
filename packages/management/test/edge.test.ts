import { afterAll, beforeAll, describe, expect, test } from "bun:test";

import { Code } from "@connectrpc/connect";

import { ensureEdgeToken } from "../src/auth/tokens.ts";
import { randomToken } from "../src/crypto.ts";
import { wakesPerMinute } from "../src/edge/service.ts";
import { coreMachineName } from "../src/environments/machines.ts";
import { reconcile, type ReconcilerOptions } from "../src/environments/reconciler.ts";
import { LogSeverity, LogSource, SleepingPingMode } from "../src/gen/chunk/management/v1/common_pb.ts";
import { EdgeService, WakeOutcome, WakeReason } from "../src/gen/chunk/management/v1/edge_pb.ts";
import { EnvironmentService } from "../src/gen/chunk/management/v1/environment_pb.ts";
import { LogService } from "../src/gen/chunk/management/v1/logs_pb.ts";
import { EnvironmentState, ProjectService } from "../src/gen/chunk/management/v1/projects_pb.ts";
import { fakeProvider } from "./fake-provider.ts";
import { codeOf, createEnvironment, databaseUrl, deployRelease, type Harness, next, startHarness } from "./harness.ts";

describe.skipIf(!databaseUrl)("EdgeService and LogService", () => {
  let h: Harness;
  let edge: ReturnType<typeof h.client<typeof EdgeService>>;
  const { provider } = fakeProvider();
  let options: ReconcilerOptions;
  beforeAll(async () => {
    h = await startHarness();
    const token = `chunk_${randomToken()}`;
    await ensureEdgeToken(h.sql, token);
    edge = h.client(EdgeService, token);
    options = { provider, image: "chunk/environment:test", managementUrl: h.url, coreMemoryMib: 1024, corePort: 7070 };
  });
  afterAll(() => h.close());

  /** A deployed environment with its core machine, attached as core. */
  async function running() {
    const { projectId, environmentId } = await createEnvironment(h);
    await deployRelease(h, projectId, environmentId, "r1");
    await reconcile(h.deps, options);
    const core = await provider.status(coreMachineName(environmentId));
    const [row] = await h.sql<{ hostname: string }[]>`select hostname from environments where id = ${environmentId}`;
    const token = (
      await h.keys.cipher.open(await machineToken(environmentId), `machine-token/${environmentId}`)
    ).toString();
    const client = h.client(EnvironmentService, token);
    const abort = new AbortController();
    const stream = client.attach({ instanceId: crypto.randomUUID(), core: true, epoch: 1n }, { signal: abort.signal });
    const messages = stream[Symbol.asyncIterator]();
    const { lease } = await next(messages);
    const revision = async () =>
      (await h.sql<{ revision: bigint }[]>`select revision from environments where id = ${environmentId}`)[0]
        ?.revision ?? 0n;
    let sequence = 0n;
    const report = async (fields: { gatewayAddresses?: string[]; readyToSuspend?: boolean; hostnamePing?: string }) =>
      client.reportStatus({
        lease,
        sequence: ++sequence,
        desiredRevision: await revision(),
        gatewayAddresses: fields.gatewayAddresses ?? [],
        readyToSuspend: fields.readyToSuspend ?? false,
        pings: fields.hostnamePing
          ? [{ hostname: `${row?.hostname.toUpperCase()}.`, statusJson: fields.hostnamePing }]
          : [],
      });
    return {
      projectId,
      environmentId,
      hostname: row?.hostname ?? "",
      coreAddress: core.addresses[0] ?? "",
      client,
      report,
      revision,
      close: () => abort.abort(),
    };
  }

  async function machineToken(environmentId: string) {
    const [row] = await h.sql<{ machine_token: Uint8Array }[]>`
      select machine_token from environments where id = ${environmentId}`;
    return row?.machine_token ?? new Uint8Array();
  }

  test("WatchRoutes sends every route, then only changes, listing gateways on provisioned machines", async () => {
    const env = await running();
    const abort = new AbortController();
    const routes = edge.watchRoutes({}, { signal: abort.signal })[Symbol.asyncIterator]();
    const first = await next(routes);
    expect(first.reset).toBe(true);
    expect(first.routes.find((route) => route.hostname === env.hostname)).toMatchObject({
      environmentId: env.environmentId,
      gatewayAddresses: [],
      asleep: false,
      sleepingPing: SleepingPingMode.CACHE,
    });

    await env.report({
      gatewayAddresses: [`${env.coreAddress}:25565`, "203.0.113.9:25565"],
      hostnamePing: '{"players":{"online":1}}',
    });
    const second = await next(routes);
    expect(second.reset).toBe(false);
    expect(second.revision).toBe(first.revision + 1n);
    expect(second.routes).toHaveLength(1);
    expect(second.routes[0]).toMatchObject({
      hostname: env.hostname,
      gatewayAddresses: [`${env.coreAddress}:25565`],
      cachedStatusJson: '{"players":{"online":1}}',
    });

    env.close();
    await h.client(ProjectService).deleteEnvironment({ environmentId: env.environmentId });
    expect((await next(routes)).removedHostnames).toEqual([env.hostname]);
    abort.abort();
  });

  test("Wake refuses blocked clients, coalesces, throttles, and advances the revision it accepts", async () => {
    const empty = await createEnvironment(h);
    const wake = (environmentId: string, clientAddress = "192.0.2.10", reason = WakeReason.LOGIN) =>
      edge.wake({ environmentId, clientAddress, reason });
    expect(await codeOf(wake(empty.environmentId))).toBe(Code.FailedPrecondition);
    expect(await codeOf(wake("env_missing"))).toBe(Code.NotFound);

    const env = await running();
    await env.report({ gatewayAddresses: [`${env.coreAddress}:25565`], readyToSuspend: true });
    await reconcile(h.deps, options);
    const state = async () =>
      (await h.client(ProjectService).getEnvironment({ environmentId: env.environmentId })).environment?.state;
    expect(await state()).toBe(EnvironmentState.SUSPENDED);

    await env.client.reportFailedAuth({ failures: [{ clientAddress: "2001:db8::1" }] });
    expect((await wake(env.environmentId, "2001:db8::ffff")).outcome).toBe(WakeOutcome.BLOCKED);
    const before = await env.revision();
    expect((await wake(env.environmentId, "192.0.2.10", WakeReason.PING)).outcome).toBe(WakeOutcome.AWAKE);
    expect(await env.revision()).toBe(before);

    expect((await wake(env.environmentId)).outcome).toBe(WakeOutcome.WAKING);
    expect((await wake(env.environmentId)).outcome).toBe(WakeOutcome.WAKING);
    expect(await env.revision()).toBe(before + 1n);
    await reconcile(h.deps, options);
    expect(await state()).toBe(EnvironmentState.STARTING);

    await env.report({ gatewayAddresses: [`${env.coreAddress}:25565`] });
    await h.sql`update environments set wake_count = ${wakesPerMinute} where id = ${env.environmentId}`;
    expect((await wake(env.environmentId)).outcome).toBe(WakeOutcome.THROTTLED);
    env.close();
  });

  test("Wake from a blocked client leaves an environment that is already running alone", async () => {
    const env = await running();
    await env.report({ gatewayAddresses: [`${env.coreAddress}:25565`] });
    const state = async () =>
      (await h.client(ProjectService).getEnvironment({ environmentId: env.environmentId })).environment?.state;
    expect(await state()).toBe(EnvironmentState.RUNNING);
    await env.client.reportFailedAuth({ failures: [{ clientAddress: "192.0.2.20" }] });

    // An edge still holding the environment's asleep route wakes for the blocked client.
    const before = await env.revision();
    const { outcome } = await edge.wake({
      environmentId: env.environmentId,
      clientAddress: "192.0.2.20",
      reason: WakeReason.LOGIN,
    });
    expect(outcome).toBe(WakeOutcome.AWAKE);
    expect(await env.revision()).toBe(before);
    env.close();
  });

  test("ReadLogs sends the newest stored entries oldest first, then follows", async () => {
    const env = await running();
    let sequence = 0n;
    const ship = (...apps: string[]) =>
      env.client.reportLogs({
        entries: apps.map((appId) => ({
          time: { seconds: BigInt(Math.floor(Date.now() / 1000)) },
          source: appId ? LogSource.JVM : LogSource.CORE,
          severity: LogSeverity.INFO,
          message: `line ${++sequence}`,
          instanceId: "core-a",
          appId,
          sequence,
        })),
      });
    await ship("", "lobby", "", "lobby");
    const logs = h.client(LogService);
    const read = async (request: { limit?: number; appId?: string }) =>
      (await Array.fromAsync(logs.readLogs({ environmentId: env.environmentId, ...request })))
        .flatMap((response) => response.entries)
        .map((entry) => entry.message);
    expect(await read({ limit: 2 })).toEqual(["line 3", "line 4"]);
    expect(await read({ appId: "lobby" })).toEqual(["line 2", "line 4"]);

    const abort = new AbortController();
    const stream = logs.readLogs(
      { environmentId: env.environmentId, appId: "lobby", limit: 1, follow: true },
      { signal: abort.signal },
    );
    const following = stream[Symbol.asyncIterator]();
    expect((await next(following)).entries.map((entry) => entry.message)).toEqual(["line 4"]);
    await ship("", "lobby");
    expect((await next(following)).entries.map((entry) => entry.message)).toEqual(["line 6"]);
    abort.abort();
    env.close();
  });

  test("following with start_time skips entries that arrive late with earlier times", async () => {
    const env = await running();
    const now = BigInt(Math.floor(Date.now() / 1000));
    const entry = (sequence: bigint, seconds: bigint) => ({
      time: { seconds },
      source: LogSource.CORE,
      severity: LogSeverity.INFO,
      message: `at ${seconds - now}`,
      instanceId: "core-a",
      sequence,
    });
    await env.client.reportLogs({ entries: [entry(1n, now)] });
    const abort = new AbortController();
    const stream = h
      .client(LogService)
      .readLogs(
        { environmentId: env.environmentId, startTime: { seconds: now }, follow: true },
        { signal: abort.signal },
      );
    const following = stream[Symbol.asyncIterator]();
    expect((await next(following)).entries.map((line) => line.message)).toEqual(["at 0"]);
    await env.client.reportLogs({ entries: [entry(2n, now - 60n), entry(3n, now + 1n)] });
    expect((await next(following)).entries.map((line) => line.message)).toEqual(["at 1"]);
    abort.abort();
    env.close();
  });
});
