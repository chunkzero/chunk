import { issueEnvironmentToken } from "../auth/tokens.ts";
import { notify } from "../changes.ts";
import type { Deps } from "../deps.ts";
import { CapacityState } from "../gen/chunk/management/v1/environment_pb.ts";
import { EnvironmentState } from "../gen/chunk/management/v1/projects_pb.ts";
import type { Provider } from "../providers/provider.ts";
import type { CapacityRow } from "./capacity.ts";
import { desiredDeployment } from "./desired.ts";
import { capacityMachineSpec, coreMachineSpec, type MachineOptions } from "./machines.ts";
import { advanceRevision } from "./store.ts";

export interface ReconcilerOptions extends MachineOptions {
  provider: Provider;
}

interface EnvironmentRow {
  id: string;
  state: EnvironmentState;
  revision: bigint;
  lease: bigint;
  ready_to_suspend: boolean;
  report_desired_revision: bigint;
  machine_id: string;
  machine_token: Uint8Array | null;
  alarm_due_seconds: bigint | null;
  alarm_due_nanos: number | null;
  alarm_fired: boolean;
}

const intervalMs = 5000;

/**
 * Drives the provider toward what the database asks for, on every change and every few seconds. One process runs
 * it; every step is idempotent, so a crash only repeats work.
 */
export function startReconciler(deps: Deps, options: ReconcilerOptions): { stop(): Promise<void> } {
  const abort = new AbortController();
  const subscription = deps.changes.subscribe((change) => change.kind === "environment");
  const running = (async () => {
    while (!abort.signal.aborted) {
      await reconcile(deps, options).catch((error: unknown) => console.error("reconciling failed:", error));
      await subscription.next(intervalMs, abort.signal);
    }
  })();
  return {
    async stop() {
      abort.abort();
      await running;
      subscription.close();
    },
  };
}

/** One pass over every environment; a failing environment does not hold up the others. */
export async function reconcile(deps: Deps, options: ReconcilerOptions): Promise<void> {
  const environments = await deps.sql<EnvironmentRow[]>`
    select id, state, revision, lease, ready_to_suspend, report_desired_revision, machine_id, machine_token,
      alarm_due_seconds, alarm_due_nanos, alarm_fired
    from environments
    order by seq`;
  for (const environment of environments) {
    try {
      await reconcileEnvironment(deps, options, environment);
    } catch (error) {
      console.error(`reconciling environment ${environment.id} failed:`, error);
    }
  }
}

async function reconcileEnvironment(deps: Deps, options: ReconcilerOptions, environment: EnvironmentRow) {
  const { sql } = deps;
  const { provider } = options;
  const id = environment.id;
  const capacity = await sql<CapacityRow[]>`
    select * from capacity_requests where environment_id = ${id} and not torn_down order by create_time`;

  if (environment.state === EnvironmentState.DELETING) {
    for (const request of capacity) if (request.machine_id) await provider.destroy(request.machine_id);
    if (environment.machine_id) await provider.destroy(environment.machine_id);
    await sql`delete from environments where id = ${id} and state = ${EnvironmentState.DELETING}`;
    return;
  }

  for (const request of capacity) {
    if (request.state === CapacityState.PROVISIONING) await provision(deps, options, environment, request);
    else if (request.state !== CapacityState.READY) await tearDown(deps, options, request);
  }
  if (!(await desiredDeployment(sql, id))) return;

  const machineId = environment.machine_id || (await createCore(deps, options, environment));
  if (!machineId) return;
  const ready = capacity.filter((request) => request.state === CapacityState.READY && request.machine_id);

  const idle =
    environment.lease > 0n &&
    environment.ready_to_suspend &&
    environment.report_desired_revision === environment.revision;
  if (idle) {
    if (environment.state === EnvironmentState.SUSPENDED) return wakeForAlarm(deps, environment);
    // Marked first and only while the idle report still holds, so a wake accepted in between resumes it next pass.
    const marked = await sql`
      update environments set state = ${EnvironmentState.SUSPENDED}
      where id = ${id} and revision = ${environment.revision} and ready_to_suspend
        and report_desired_revision = revision and state <> ${EnvironmentState.DELETING}`;
    if (marked.count === 0) return;
    await notify(sql, { kind: "environment", environmentId: id });
    for (const request of ready) await provider.suspend(request.machine_id);
    await provider.suspend(machineId);
    return;
  }

  if (environment.state === EnvironmentState.RUNNING) return;
  await provider.start(machineId);
  for (const request of ready) await provider.start(request.machine_id);
  if (environment.state !== EnvironmentState.STARTING) {
    await sql`
      update environments set state = ${EnvironmentState.STARTING}
      where id = ${id} and state = ${environment.state}`;
    await notify(sql, { kind: "environment", environmentId: id });
  }
}

/** Creates core's machine with the environment's token, issuing one first. Returns the machine ID once saved. */
async function createCore(deps: Deps, options: ReconcilerOptions, environment: EnvironmentRow) {
  const { sql, keys } = deps;
  const context = `machine-token/${environment.id}`;
  // The token is saved before the machine exists, so a retry after a crash builds the same machine.
  let token =
    environment.machine_token && new TextDecoder().decode(await keys.cipher.open(environment.machine_token, context));
  if (!token) {
    token = await issueEnvironmentToken(sql, environment.id);
    await sql`
      update environments set machine_token = ${await keys.cipher.seal(new TextEncoder().encode(token), context)}
      where id = ${environment.id}`;
  }
  const machine = await options.provider.create(coreMachineSpec(options, environment.id, token));
  const saved = await sql`
    update environments set machine_id = ${machine.id}, machine_addresses = ${sql.array(machine.addresses)}::text[]
    where id = ${environment.id} and state <> ${EnvironmentState.DELETING}`;
  if (saved.count === 0) {
    await options.provider.destroy(machine.id);
    return undefined;
  }
  return machine.id;
}

/** Creates and starts an extra machine. A provider error fails the request for good; core retries with a new ID. */
async function provision(deps: Deps, options: ReconcilerOptions, environment: EnvironmentRow, request: CapacityRow) {
  const { sql, keys } = deps;
  const { provider } = options;
  const where = sql`environment_id = ${request.environment_id} and request_id = ${request.request_id}`;
  try {
    if (!environment.machine_id || !environment.machine_token) throw new Error("core has no machine yet");
    const core = await provider.status(environment.machine_id);
    const coreHost = core.addresses[0];
    if (!coreHost) throw new Error("core's machine has no address");
    const environmentToken = new TextDecoder().decode(
      await keys.cipher.open(environment.machine_token, `machine-token/${environment.id}`),
    );
    const spec = capacityMachineSpec(options, request, {
      coreAddress: `${coreHost}:${options.corePort}`,
      environmentToken,
    });
    const machine = await provider.create(spec);
    const saved = await sql`update capacity_requests set machine_id = ${machine.id} where ${where}`;
    if (saved.count === 0) return provider.destroy(machine.id);
    const started = await provider.start(machine.id);
    await sql`
      update capacity_requests
      set state = ${CapacityState.READY}, machine_addresses = ${sql.array(started.addresses)}::text[]
      where ${where} and state = ${CapacityState.PROVISIONING}`;
  } catch (error) {
    console.error(`provisioning capacity ${request.request_id} failed:`, error);
    await sql`
      update capacity_requests
      set state = ${CapacityState.FAILED}, message = ${error instanceof Error ? error.message : String(error)}
      where ${where} and state = ${CapacityState.PROVISIONING}`;
  }
  await notify(sql, { kind: "environment", environmentId: request.environment_id });
}

async function tearDown({ sql }: Deps, { provider }: ReconcilerOptions, request: CapacityRow) {
  if (request.machine_id) await provider.destroy(request.machine_id);
  await sql`
    update capacity_requests set torn_down = true
    where environment_id = ${request.environment_id} and request_id = ${request.request_id}`;
}

/** Wakes a suspended environment once its alarm is due, by invalidating the idle report it suspended on. */
async function wakeForAlarm({ sql }: Deps, environment: EnvironmentRow) {
  if (environment.alarm_due_seconds === null || environment.alarm_fired) return;
  const dueMs = Number(environment.alarm_due_seconds) * 1000 + (environment.alarm_due_nanos ?? 0) / 1e6;
  if (dueMs > Date.now()) return;
  await sql.begin(async (tx) => {
    await tx`update environments set alarm_fired = true where id = ${environment.id}`;
    await advanceRevision(tx, environment.id);
  });
}
