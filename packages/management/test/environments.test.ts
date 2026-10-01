import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";

import { Code } from "@connectrpc/connect";

import { issueEnvironmentToken } from "../src/auth/tokens.ts";
import type { Sql } from "../src/db.ts";
import { claimLease } from "../src/environments/store.ts";
import { DeploymentState, LogSeverity, LogSource } from "../src/gen/chunk/management/v1/common_pb.ts";
import { DeploymentService } from "../src/gen/chunk/management/v1/deployments_pb.ts";
import { EnvironmentService } from "../src/gen/chunk/management/v1/environment_pb.ts";
import { ProjectService } from "../src/gen/chunk/management/v1/projects_pb.ts";
import { SecretService } from "../src/gen/chunk/management/v1/secrets_pb.ts";
import { codeOf, createEnvironment, databaseUrl, deployRelease, type Harness, startHarness } from "./harness.ts";

describe.skipIf(!databaseUrl)("EnvironmentService", () => {
  let h: Harness;
  beforeAll(async () => {
    h = await startHarness();
  });
  afterAll(() => h.close());

  async function environment() {
    const { projectId, environmentId } = await createEnvironment(h);
    const token = await issueEnvironmentToken(h.sql, environmentId);
    return { projectId, environmentId, client: h.client(EnvironmentService, token), token };
  }

  function attach(
    client: ReturnType<typeof h.client<typeof EnvironmentService>>,
    request: { core?: boolean; epoch?: bigint; instanceId?: string },
  ) {
    const abort = new AbortController();
    const stream = client.attach({ instanceId: crypto.randomUUID(), ...request }, { signal: abort.signal });
    return { messages: stream[Symbol.asyncIterator](), close: () => abort.abort() };
  }

  test("tokens only reach the services of their kind", async () => {
    const { environmentId, token } = await environment();
    expect(await codeOf(h.client(ProjectService, token).getEnvironment({ environmentId }))).toBe(Code.PermissionDenied);
    expect(await codeOf(h.client(EnvironmentService).reportUsage({}))).toBe(Code.PermissionDenied);
  });

  test("attach streams the complete desired state again after every change", async () => {
    const { projectId, environmentId, client } = await environment();
    const deploymentId = await deployRelease(h, projectId, environmentId, "r1");
    const stream = attach(client, {});
    const first = (await stream.messages.next()).value;
    expect(first).toMatchObject({ environmentId, projectId, deploymentId, lease: 0n, stopPrevious: false });
    expect(first.drain).toMatchObject({ maxAgeSeconds: 10_800, deadlineSeconds: 14_400 });
    const download = await fetch(first.release.url);
    expect(
      createHash("sha256")
        .update(await download.bytes())
        .digest("hex"),
    ).toBe(first.release.sha256);

    await h.client(SecretService).setSecret({
      requestId: crypto.randomUUID(),
      environmentId,
      name: "API_KEY",
      value: new TextEncoder().encode("hunter2"),
    });
    const second = (await stream.messages.next()).value;
    expect(second.revision).toBeGreaterThan(first.revision);
    expect(second.deploymentId).toBe(deploymentId);
    expect(new TextDecoder().decode(second.secrets[0]?.value)).toBe("hunter2");
    stream.close();
  });

  test("a newer core attach supersedes the owner for good", async () => {
    const { client } = await environment();
    const a = attach(client, { core: true, epoch: 1n, instanceId: "core-a" });
    expect((await a.messages.next()).value.lease).toBe(1n);
    const b = attach(client, { core: true, epoch: 1n, instanceId: "core-b" });
    expect((await b.messages.next()).value.lease).toBe(2n);
    expect(await codeOf(a.messages.next())).toBe(Code.FailedPrecondition);

    const again = attach(client, { core: true, epoch: 2n, instanceId: "core-a" });
    expect(await codeOf(again.messages.next())).toBe(Code.FailedPrecondition);
    const older = attach(client, { core: true, epoch: 0n, instanceId: "core-c" });
    expect(await codeOf(older.messages.next())).toBe(Code.FailedPrecondition);
    expect(await codeOf(client.reportStatus({ lease: 1n, sequence: 1n }))).toBe(Code.FailedPrecondition);
    expect(await codeOf(client.setWakeAlarm({ lease: 1n, epoch: 1n, generation: 1n }))).toBe(Code.FailedPrecondition);
    b.close();
  });

  test("status reports apply in sequence order and drive deployment progress", async () => {
    const { projectId, environmentId, client } = await environment();
    const deploymentId = await deployRelease(h, projectId, environmentId, "r1");
    const core = attach(client, { core: true, epoch: 1n });
    const { lease, revision } = (await core.messages.next()).value;
    const report = (sequence: bigint, state: DeploymentState) =>
      client.reportStatus({
        lease,
        sequence,
        desiredRevision: revision,
        gatewayAddresses: ["10.0.0.2:25565"],
        onlinePlayers: 3,
        deployment: { deploymentId, state },
      });

    await report(2n, DeploymentState.ACTIVE);
    await report(1n, DeploymentState.FAILED);
    const deployment = await h.client(DeploymentService).getDeployment({ deploymentId });
    expect(deployment.deployment?.state).toBe(DeploymentState.ACTIVE);
    const { environment: env } = await h.client(ProjectService).getEnvironment({ environmentId });
    expect(env).toMatchObject({ activeDeploymentId: deploymentId, onlinePlayers: 3 });

    expect(await codeOf(client.reportStatus({ lease, sequence: 3n, gatewayAddresses: ["10.0.0.2"] }))).toBe(
      Code.InvalidArgument,
    );
    core.close();
  });

  test("wake alarms are ordered by epoch, then generation, and echo what is stored", async () => {
    const { client } = await environment();
    const core = attach(client, { core: true, epoch: 1n });
    const { lease } = (await core.messages.next()).value;
    const due = (seconds: bigint, nanos = 0) => ({ seconds, nanos });
    const set = (epoch: bigint, generation: bigint, dueTime?: ReturnType<typeof due>) =>
      client.setWakeAlarm({ lease, epoch, generation, ...(dueTime ? { dueTime } : {}) });

    expect(await set(1n, 5n, due(2_000_000_000n, 7))).toMatchObject({
      epoch: 1n,
      generation: 5n,
      dueTime: due(2_000_000_000n, 7),
    });
    expect(await set(1n, 4n, due(1_000n))).toMatchObject({ generation: 5n, dueTime: due(2_000_000_000n, 7) });
    expect(await set(1n, 5n, due(2_000_000_000n, 7))).toMatchObject({ generation: 5n });
    expect(await codeOf(set(1n, 5n, due(2_000_000_000n, 8)))).toBe(Code.FailedPrecondition);
    const cleared = await set(2n, 0n);
    expect(cleared).toMatchObject({ epoch: 2n, generation: 0n });
    expect(cleared.dueTime).toBeUndefined();
    core.close();
  });

  test("reports deduplicate retried batches", async () => {
    const { environmentId, client } = await environment();
    const time = { seconds: 1_700_000_000n, nanos: 0 };
    const entries = [1n, 2n].map((sequence) => ({
      time,
      source: LogSource.CORE,
      severity: LogSeverity.INFO,
      message: `line ${sequence}`,
      instanceId: "core-a",
      sequence,
    }));
    await client.reportLogs({ entries });
    await client.reportLogs({ entries });
    // Only an owner's usage counts, and only from its takeover on.
    const core = attach(client, { core: true, epoch: 1n, instanceId: "core-a" });
    await core.messages.next();
    const record = {
      id: "u1",
      instanceId: "core-a",
      startTime: time,
      endTime: { seconds: 4_000_000_000n, nanos: 0 },
      playerSeconds: 60n,
    };
    await client.reportUsage({ records: [record] });
    await client.reportUsage({ records: [record] });
    core.close();
    await client.reportFailedAuth({
      failures: [{ clientAddress: "2001:db8:1:2:3:4:5:6" }, { clientAddress: "::ffff:192.0.2.1" }],
    });
    expect(await codeOf(client.reportFailedAuth({ failures: [{ clientAddress: "nope" }] }))).toBe(Code.InvalidArgument);

    const [counts] = await h.sql<{ logs: bigint; usage: bigint }[]>`
      select (select count(*) from log_entries where environment_id = ${environmentId}) as logs,
        (select count(*) from usage_records where environment_id = ${environmentId}) as usage`;
    expect(counts).toEqual({ logs: 2n, usage: 1n });
    const blocked = await h.sql<{ address: string }[]>`
      select address from blocked_addresses where environment_id = ${environmentId} order by address`;
    expect(blocked.map((row) => row.address)).toEqual(["192.0.2.1", "2001:db8:1:2::/64"]);
  });

  test("usage is cut at the takeover on management's clock, whatever the cores' clocks say", async () => {
    const { environmentId, client } = await environment();
    const a = attach(client, { core: true, epoch: 1n, instanceId: "core-a" });
    await a.messages.next();
    const b = attach(client, { core: true, epoch: 1n, instanceId: "core-b" });
    await b.messages.next();
    // Pinned, so the expected spans are whole seconds.
    const takeover = 1_800_000_000n;
    await h.sql`update superseded_instances set superseded_time = to_timestamp(${takeover}) where instance_id = 'core-a'`;
    await h.sql`update environments set owner_since = to_timestamp(${takeover}) where id = ${environmentId}`;
    const span = (id: string, instanceId: string, from: bigint, to: bigint) => ({
      id,
      instanceId,
      startTime: { seconds: takeover + from, nanos: 0 },
      endTime: { seconds: takeover + to, nanos: 0 },
      playerSeconds: 2n * (to - from),
    });
    // A counted on after B took over, as it had not heard of B yet, and B's clock runs 4 seconds behind.
    await client.reportUsage({
      records: [span("a/1", "core-a", -30n, 30n), span("a/2", "core-a", 30n, 40n), span("b/1", "core-b", -4n, 26n)],
    });
    expect(await usage(environmentId, takeover)).toEqual([
      { id: "a/1", start: -30n, end: 0n, player_seconds: 60n },
      { id: "b/1", start: 0n, end: 26n, player_seconds: 52n },
    ]);
    a.close();
    b.close();
  });

  test("a takeover whose transaction began before an earlier one's still never precedes it", async () => {
    const { environmentId } = await environment();
    await claimLease(h.sql, environmentId, "core-a", 1n);
    // C's transaction begins, then waits until B has taken over and committed.
    let release = () => {};
    const gate = new Promise<void>((resolve) => (release = resolve));
    let begun = () => {};
    const started = new Promise<void>((resolve) => (begun = resolve));
    const delayed = {
      begin: (run: (tx: unknown) => Promise<unknown>) =>
        h.sql.begin(async (tx) => {
          await tx`select 1`;
          begun();
          await gate;
          return run(tx);
        }),
    } as unknown as Sql;
    const c = claimLease(delayed, environmentId, "core-c", 1n);
    await started;
    await claimLease(h.sql, environmentId, "core-b", 1n);
    release();
    await c;

    const windows = await h.sql<{ instance_id: string; from: string; until: string | null }[]>`
      select instance_id, owned_since::text as from, superseded_time::text as until from superseded_instances
      where environment_id = ${environmentId}
      union all
      select owner_instance_id, owner_since::text, null from environments where id = ${environmentId}
      order by instance_id`;
    expect(windows.map((window) => window.instance_id)).toEqual(["core-a", "core-b", "core-c"]);
    const [a, b, c2] = windows;
    // Each window ends where the next begins, and none ends before it began.
    expect([a!.until, b!.until]).toEqual([b!.from, c2!.from]);
    const [ordered] = await h.sql<{ ok: boolean }[]>`
      select ${a!.from}::timestamptz <= ${a!.until}::timestamptz
        and ${b!.from}::timestamptz <= ${b!.until}::timestamptz as ok`;
    expect(ordered?.ok).toBe(true);
  });

  test("a takeover cuts the spans its predecessor stored already", async () => {
    const { environmentId, client } = await environment();
    const a = attach(client, { core: true, epoch: 1n, instanceId: "core-a" });
    await a.messages.next();
    // A's clock runs ahead, so a span it reported before the takeover ends after it. A has owned it for a while.
    const now = BigInt(Math.floor(Date.now() / 1000));
    await h.sql`update environments set owner_since = to_timestamp(${now - 120n}) where id = ${environmentId}`;
    const span = (id: string, from: bigint, to: bigint) => ({
      id,
      instanceId: "core-a",
      startTime: { seconds: now + from, nanos: 0 },
      endTime: { seconds: now + to, nanos: 0 },
      playerSeconds: to - from,
    });
    await client.reportUsage({ records: [span("a/1", -60n, 3_600n), span("a/2", 3_600n, 3_660n)] });
    const b = attach(client, { core: true, epoch: 1n, instanceId: "core-b" });
    await b.messages.next();
    const [row] = await h.sql<{ superseded: bigint }[]>`
      select floor(extract(epoch from superseded_time))::bigint as superseded from superseded_instances
      where environment_id = ${environmentId} and instance_id = 'core-a'`;
    const end = row!.superseded - now;
    // One player throughout, so the cut span keeps one player second per whole second.
    expect(await usage(environmentId, now)).toEqual([{ id: "a/1", start: -60n, end, player_seconds: end + 60n }]);
    a.close();
    b.close();
  });

  /** The environment's stored spans, in whole seconds from `origin`. */
  async function usage(environmentId: string, origin: bigint) {
    const rows = await h.sql<{ id: string; start: bigint; end: bigint; player_seconds: bigint }[]>`
      select id, floor(extract(epoch from start_time))::bigint - ${origin} as start,
        floor(extract(epoch from end_time))::bigint - ${origin} as end, player_seconds
      from usage_records where environment_id = ${environmentId} order by id`;
    return [...rows];
  }

  test("a string holding NUL is refused before it reaches Postgres", async () => {
    const { client } = await environment();
    const entry = {
      time: { seconds: 1_700_000_000n, nanos: 0 },
      message: "before\0after",
      instanceId: "core-a",
      sequence: 1n,
    };
    expect(await codeOf(client.reportLogs({ entries: [entry] }))).toBe(Code.InvalidArgument);
    expect(await codeOf(attach(client, { core: true, epoch: 1n, instanceId: "core\0a" }).messages.next())).toBe(
      Code.InvalidArgument,
    );
  });
});
