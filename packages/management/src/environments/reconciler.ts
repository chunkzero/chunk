import { issueEnvironmentToken } from "../auth/tokens.ts";
import { notify } from "../changes.ts";
import type { Deps } from "../deps.ts";
import { CapacityState } from "../gen/chunk/management/v1/environment_pb.ts";
import { EnvironmentState } from "../gen/chunk/management/v1/projects_pb.ts";
import type { Machine, Provider } from "../providers/provider.ts";
import type { CapacityRow } from "./capacity.ts";
import { desiredDeployment } from "./desired.ts";
import {
  capacityMachineName,
  capacityMachineSpec,
  coreMachineName,
  coreMachineSpec,
  type MachineOptions,
} from "./machines.ts";
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
  machine_addresses: string[];
  machine_token: Uint8Array | null;
  alarm_epoch: bigint;
  alarm_generation: bigint;
  alarm_due_seconds: bigint | null;
  alarm_due_nanos: number | null;
  alarm_fired: boolean;
}

const intervalMs = 5000;

/**
 * Drives the provider toward what the database asks for, on every change and every few seconds. One process runs
 * it. Each pass compares the machines' observed state with the desired one, so a step that failed or was cut short
 * is retried until they agree.
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
    select id, state, revision, lease, ready_to_suspend, report_desired_revision, machine_id, machine_addresses,
      machine_token, alarm_epoch, alarm_generation, alarm_due_seconds, alarm_due_nanos, alarm_fired
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
    for (const request of capacity) await provider.destroy(capacityMachineName(request));
    await provider.destroy(coreMachineName(id));
    await sql`delete from environments where id = ${id} and state = ${EnvironmentState.DELETING}`;
    return;
  }
  for (const request of capacity) {
    if (request.state === CapacityState.FAILED || request.state === CapacityState.RELEASED) {
      await tearDown(deps, options, request);
    }
  }
  if (!(await desiredDeployment(sql, id))) return;

  const observed = environment.machine_id ? await provider.status(environment.machine_id) : undefined;
  let core = observed && observed.state !== "missing" ? observed : await createCore(deps, options, environment);
  if (!core) return;
  const active = capacity.filter(
    (request) => request.state === CapacityState.PROVISIONING || request.state === CapacityState.READY,
  );

  const idle =
    environment.lease > 0n &&
    environment.ready_to_suspend &&
    environment.report_desired_revision === environment.revision;
  if (idle) {
    // Checked against the latest accepted report on every pass, retries included, since core may have reported
    // activity or accepted a wake after this pass read the environment. A later change resumes it next pass.
    const [marked] = await sql<{ changed: boolean }[]>`
      update environments set state = ${EnvironmentState.SUSPENDED}
      where id = ${id} and revision = ${environment.revision} and lease > 0 and ready_to_suspend
        and report_desired_revision = revision and state <> ${EnvironmentState.DELETING}
      returning ${environment.state !== EnvironmentState.SUSPENDED} as changed`;
    if (!marked) return;
    if (marked.changed) await notify(sql, { kind: "environment", environmentId: id });
    // Reports, wakes and attaches lock the row too, so none can land between this recheck and the suspension.
    const suspendIfIdle = (machineId: string) =>
      sql.begin(async (tx) => {
        const [still] = await tx`
          select 1 as idle from environments
          where id = ${id} and state = ${EnvironmentState.SUSPENDED} and revision = ${environment.revision}
            and lease = ${environment.lease} and ready_to_suspend and report_desired_revision = revision
          for update`;
        if (still) await provider.suspend(machineId);
        return Boolean(still);
      });
    for (const request of active) {
      if (!request.machine_id) continue;
      const machine = await provider.status(request.machine_id);
      if (machine.state === "running" && !(await suspendIfIdle(machine.id))) return;
    }
    if (core.state === "running" && !(await suspendIfIdle(core.id))) return;
    await fireDueAlarm(deps, environment);
    return;
  }

  if (core.state !== "running") core = await provider.start(core.id);
  await saveCoreAddresses(deps, environment, core);
  if (environment.state === EnvironmentState.PENDING || environment.state === EnvironmentState.SUSPENDED) {
    await sql`
      update environments set state = ${EnvironmentState.STARTING}
      where id = ${id} and state = ${environment.state}`;
    await notify(sql, { kind: "environment", environmentId: id });
  }
  for (const request of active) await keepRunning(deps, options, environment, core, request);
}

/** Creates core's machine with the environment's token, issuing one first. Returns the machine once saved. */
async function createCore(deps: Deps, options: ReconcilerOptions, environment: EnvironmentRow) {
  const { sql, keys } = deps;
  const context = `machine-token/${environment.id}`;
  // The token is saved before the machine exists, so a retry after a crash builds the same machine, and in the same
  // transaction as the row lock deletion takes, so a deleted or deleting environment never gets a machine.
  const { sealed } = await sql.begin(async (tx) => {
    const [row] = await tx<{ machine_token: Uint8Array | null }[]>`
      select machine_token from environments
      where id = ${environment.id} and state <> ${EnvironmentState.DELETING}
      for update`;
    if (!row || row.machine_token) return { sealed: row?.machine_token ?? undefined };
    const issued = await issueEnvironmentToken(tx, environment.id);
    const token = await keys.cipher.seal(new TextEncoder().encode(issued), context);
    await tx`update environments set machine_token = ${token} where id = ${environment.id}`;
    return { sealed: token };
  });
  if (!sealed) return undefined;
  const token = new TextDecoder().decode(await keys.cipher.open(sealed, context));
  const machine = await options.provider.create(coreMachineSpec(options, environment.id, token));
  const saved = await sql`
    update environments set machine_id = ${machine.id}
    where id = ${environment.id} and state <> ${EnvironmentState.DELETING}`;
  if (saved.count === 0) {
    await options.provider.destroy(machine.name);
    return undefined;
  }
  return machine;
}

/** Addresses can change whenever a machine starts, and routes only list gateways on the current ones. */
async function saveCoreAddresses({ sql }: Deps, environment: EnvironmentRow, core: Machine) {
  if (sameList(environment.machine_addresses, core.addresses)) return;
  await sql`
    update environments set machine_addresses = ${sql.array(core.addresses)}::text[]
    where id = ${environment.id}`;
  await notify(sql, { kind: "environment", environmentId: environment.id });
}

/**
 * Keeps an extra machine running: resumes a suspended one and replaces one that is missing or exited, since extra
 * machines are stateless and their join tokens expire. A provider error fails the request for good; core retries
 * with a new request ID.
 */
async function keepRunning(
  deps: Deps,
  options: ReconcilerOptions,
  environment: EnvironmentRow,
  core: Machine,
  request: CapacityRow,
) {
  const { sql, keys } = deps;
  const { provider } = options;
  const where = sql`environment_id = ${request.environment_id} and request_id = ${request.request_id}`;
  try {
    let machine = request.machine_id ? await provider.status(request.machine_id) : undefined;
    if (machine?.state === "suspended") machine = await provider.start(machine.id);
    if (machine?.state !== "running") {
      const coreHost = core.addresses[0];
      if (!coreHost || !environment.machine_token) return;
      const environmentToken = new TextDecoder().decode(
        await keys.cipher.open(environment.machine_token, `machine-token/${environment.id}`),
      );
      // A leftover from an earlier attempt, perhaps one whose create reply was lost, holds the name.
      await provider.destroy(capacityMachineName(request));
      const created = await provider.create(
        capacityMachineSpec(options, request, { coreAddress: `${coreHost}:${options.corePort}`, environmentToken }),
      );
      const saved = await sql`update capacity_requests set machine_id = ${created.id} where ${where}`;
      if (saved.count === 0) return provider.destroy(created.name);
      machine = await provider.start(created.id);
    }
    if (request.state === CapacityState.READY && sameList(request.machine_addresses, machine.addresses)) return;
    await sql`
      update capacity_requests
      set state = ${CapacityState.READY}, machine_addresses = ${sql.array(machine.addresses)}::text[]
      where ${where} and state in (${CapacityState.PROVISIONING}, ${CapacityState.READY})`;
  } catch (error) {
    console.error(`running capacity ${request.request_id} failed:`, error);
    await sql`
      update capacity_requests
      set state = ${CapacityState.FAILED}, message = ${error instanceof Error ? error.message : String(error)}
      where ${where} and state in (${CapacityState.PROVISIONING}, ${CapacityState.READY})`;
  }
  await notify(sql, { kind: "environment", environmentId: request.environment_id });
}

/** Removes a released or failed request's machine by name, which also finds one whose ID was never saved. */
async function tearDown({ sql }: Deps, { provider }: ReconcilerOptions, request: CapacityRow) {
  await provider.destroy(capacityMachineName(request));
  await sql`
    update capacity_requests set torn_down = true
    where environment_id = ${request.environment_id} and request_id = ${request.request_id}`;
}

/**
 * Wakes a suspended environment once its alarm is due, by invalidating the idle report it suspended on. Only the
 * alarm read at the start of the pass is fired; a replacement stored since then is left for its own due time.
 */
async function fireDueAlarm({ sql }: Deps, environment: EnvironmentRow) {
  const { alarm_due_seconds: seconds, alarm_due_nanos: nanos } = environment;
  if (seconds === null || environment.alarm_fired) return;
  if (Number(seconds) * 1000 + (nanos ?? 0) / 1e6 > Date.now()) return;
  await sql.begin(async (tx) => {
    const fired = await tx`
      update environments set alarm_fired = true
      where id = ${environment.id} and not alarm_fired
        and alarm_epoch = ${environment.alarm_epoch} and alarm_generation = ${environment.alarm_generation}
        and alarm_due_seconds = ${seconds} and alarm_due_nanos is not distinct from ${nanos}`;
    if (fired.count > 0) await advanceRevision(tx, environment.id);
  });
}

function sameList(a: string[], b: string[]): boolean {
  return a.length === b.length && a.every((item, i) => item === b[i]);
}
