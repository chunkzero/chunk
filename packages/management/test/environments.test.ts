import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";

import { Code } from "@connectrpc/connect";

import { issueEnvironmentToken } from "../src/auth/tokens.ts";
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
    expect(first).toMatchObject({ environmentId, projectId, deploymentId, lease: 0n });
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
    const usage = { id: "u1", startTime: time, endTime: { seconds: 1_700_000_060n, nanos: 0 }, playerSeconds: 60n };
    await client.reportUsage({ records: [usage] });
    await client.reportUsage({ records: [usage] });
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
});
