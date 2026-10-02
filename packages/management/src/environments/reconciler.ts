import { and, asc, eq, exists, gt, inArray, ne, not, sql } from "drizzle-orm";

import { untilAborted } from "../abort.ts";
import { issueEnvironmentToken } from "../auth/tokens.ts";
import { notify } from "../changes.ts";
import { advisoryLock, type Database, type Db } from "../db.ts";
import type { Deps } from "../deps.ts";
import { CapacityState, Workload } from "../gen/chunk/management/v1/environment_pb.ts";
import { EnvironmentState } from "../gen/chunk/management/v1/projects_pb.ts";
import { boundedProvider, type ProviderTimeouts, ProviderTimeoutError } from "../providers/bounded.ts";
import { type Machine, NoCapacityError, type Provider } from "../providers/provider.ts";
import { capacityRequests, environments, reconcilerLeader } from "../schema.ts";
import { type CapacityRow, capacityCredentialContext } from "./capacity.ts";
import { desiredDeployment } from "./desired.ts";
import {
  capacityMachineName,
  capacityMachineSpec,
  coreHostOf,
  coreMachineName,
  coreMachineSpec,
  type MachineOptions,
} from "./machines.ts";
import { keyedPool, type RetryBackoff, retryBackoff } from "./scheduling.ts";
import { advanceRevision } from "./store.ts";

export interface ReconcilerOptions extends MachineOptions {
  provider: Provider;
  /** How many environments the leader works on at once, and how many it tears down released machines for alongside. */
  concurrency: number;
  /** How long each provider call may take before it counts as a transient failure. */
  timeouts: ProviderTimeouts;
  /** How long a capacity request retries transient provider failures before it fails. */
  capacityRetryMs: number;
}

const environmentColumns = {
  id: environments.id,
  state: environments.state,
  revision: environments.revision,
  lease: environments.lease,
  ready_to_suspend: environments.ready_to_suspend,
  report_desired_revision: environments.report_desired_revision,
  machine_id: environments.machine_id,
  machine_addresses: environments.machine_addresses,
  machine_token: environments.machine_token,
  alarm_epoch: environments.alarm_epoch,
  alarm_generation: environments.alarm_generation,
  alarm_due_seconds: environments.alarm_due_seconds,
  alarm_due_nanos: environments.alarm_due_nanos,
  alarm_fired: environments.alarm_fired,
  forked_from_environment_id: environments.forked_from_environment_id,
};

type EnvironmentRow = Pick<typeof environments.$inferSelect, keyof typeof environmentColumns>;

const intervalMs = 5000;
/** The advisory lock the leading reconciler holds. */
const leaderLock = 0x63686e6b;

/** A newer leader epoch refused one of a pass's transactions; the pass stops at once. */
export class Superseded extends Error {
  constructor() {
    super("another reconciler took over");
    this.name = "Superseded";
  }
}

/**
 * Runs `body` in a transaction that first checks the pass's leader epoch is current and keeps a shared lock on it, so
 * a newer leader's bump waits for the transaction to end. Every write a pass makes runs in one. Provider calls run
 * outside them, since neither a lost connection nor a timeout stops a call under way: intent is committed before the
 * call instead, and calls address machines by ID wherever a name can be reused.
 */
type Fence = <T>(body: (tx: Db) => Promise<T>) => Promise<T>;

/** What one environment's run works with. */
interface Run {
  deps: Deps;
  /** With the provider's calls bounded. */
  options: ReconcilerOptions;
  fenced: Fence;
  retries: RetryBackoff;
  /** Aborts when the reconciler stops. */
  signal: AbortSignal | undefined;
}

/** A timeout or no room: the call may be retried, and the step backs off until it succeeds or the bound passes. */
function isTransient(error: unknown): error is Error {
  return error instanceof ProviderTimeoutError || error instanceof NoCapacityError;
}

/**
 * Drives the provider toward what the database asks for: every environment every few seconds, and a changed one at
 * once. Only the process holding the leader lock, on a session of its own at `databaseUrl`, leads; others retry taking
 * it on the same schedule. Leading means holding the current leader epoch, which fences out the writes of a previous
 * leader's pass still under way. Each run compares the machines' observed state with the desired one, so a step that
 * failed or was cut short is retried until they agree. Passes are not awaited, so a slow environment holds up only its
 * own runs.
 */
export function startReconciler(
  deps: Deps,
  options: ReconcilerOptions,
  databaseUrl: string,
): { stop(): Promise<void> } {
  const abort = new AbortController();
  const changed = new Set<string>();
  const subscription = deps.changes.subscribe((change) => {
    if (change.kind !== "environment") return false;
    changed.add(change.environmentId);
    return true;
  });
  const leader = advisoryLock(databaseUrl, leaderLock);
  const reconciler = createReconciler(deps, options, abort.signal);
  let epoch: bigint | undefined;
  let lastFullPass = 0;
  const running = (async () => {
    while (!abort.signal.aborted) {
      const environmentIds = [...changed];
      changed.clear();
      let waitMs = intervalMs;
      try {
        if (await leader.hold()) {
          // Only the lock holder bumps, so another epoch means this process lost the lock since it last bumped.
          const [current] = await deps.db.select({ epoch: reconcilerLeader.epoch }).from(reconcilerLeader);
          if (epoch === undefined || current?.epoch !== epoch) {
            epoch = await takeLeadership(deps.db);
            lastFullPass = 0;
          }
          const full = Date.now() - lastFullPass >= intervalMs;
          if (full) lastFullPass = Date.now();
          if (full || environmentIds.length > 0) {
            reconciler.pass(epoch, full ? undefined : environmentIds).catch((error: unknown) => {
              if (!(error instanceof Superseded)) console.error("reconciling failed:", error);
            });
          }
          waitMs = lastFullPass + intervalMs - Date.now();
        } else {
          epoch = undefined;
        }
      } catch (error) {
        console.error("reconciling failed:", error);
      }
      await subscription.next(waitMs, abort.signal);
    }
    await reconciler.idle();
    await leader.close();
  })();
  return {
    async stop() {
      abort.abort();
      await running;
      subscription.close();
    },
  };
}

/** Starts a new leader epoch and returns it. */
export async function takeLeadership(db: Db): Promise<bigint> {
  const [row] = await db
    .update(reconcilerLeader)
    .set({ epoch: sql`${reconcilerLeader.epoch} + 1` })
    .returning({ epoch: reconcilerLeader.epoch });
  if (!row) throw new Error("reconciler_leader has no row");
  return row.epoch;
}

export interface Reconciler {
  /**
   * Schedules a run under leader epoch `epoch` for each of `environmentIds`, or for every environment and a sweep of
   * untracked machines when omitted, and resolves once they have all settled. Runs share a pool of
   * `options.concurrency`, and an environment never has two at once, so a slow or failing environment holds up only its
   * own. Each environment's failed and releasing requests are torn down by runs in a pool of their own, so a request
   * released while the environment's run waits on a provider call, such as core's suspension, is torn down at once.
   * Rejects with `Superseded` when a run was superseded.
   */
  pass(epoch: bigint, environmentIds?: string[]): Promise<void>;
  /** Resolves once no run is scheduled or under way. */
  idle(): Promise<void>;
}

/**
 * A reconciler whose provider calls are bounded by `options.timeouts`, and abandoned when `signal` aborts. A call it
 * gave up on is a transient failure, retried by a later run, and never taken to have succeeded: the next run observes
 * the machines again.
 */
export function createReconciler(deps: Deps, options: ReconcilerOptions, signal?: AbortSignal): Reconciler {
  const bounded = { ...options, provider: boundedProvider(options.provider, options.timeouts, signal) };
  const pool = keyedPool(options.concurrency);
  const teardowns = keyedPool(options.concurrency);
  const retries = retryBackoff(options.capacityRetryMs);
  const guarded = (what: string, work: () => Promise<void>) => async () => {
    if (signal?.aborted) return;
    try {
      await work();
    } catch (error) {
      if (error instanceof Superseded) throw error;
      console.error(`${what} failed:`, error);
    }
  };
  return {
    async pass(epoch, environmentIds) {
      const run: Run = { deps, options: bounded, fenced: fence(deps.db, epoch), retries, signal };
      const ids =
        environmentIds ??
        (await deps.db.select({ id: environments.id }).from(environments).orderBy(environments.seq)).map(
          ({ id }) => id,
        );
      const runs = ids.flatMap((id) => [
        pool.schedule(
          `environment/${id}`,
          guarded(`reconciling environment ${id}`, () => reconcileEnvironment(run, id)),
        ),
        teardowns.schedule(
          `environment/${id}`,
          guarded(`tearing down environment ${id}'s capacity`, () => tearDownReleased(run, id)),
        ),
      ]);
      if (!environmentIds) {
        const sweeping = guarded("sweeping", () => sweep(run));
        runs.push(pool.schedule("sweep", sweeping));
      }
      const failed = (await Promise.allSettled(runs)).find((result) => result.status === "rejected");
      if (failed) throw failed.reason;
    },
    async idle() {
      await Promise.all([pool.idle(), teardowns.idle()]);
    },
  };
}

/** One pass over every environment under leader epoch `epoch`, with a reconciler of its own. */
export function reconcile(deps: Deps, options: ReconcilerOptions, epoch: bigint): Promise<void> {
  return createReconciler(deps, options).pass(epoch);
}

function fence(db: Database, epoch: bigint): Fence {
  return (body) =>
    db.transaction(async (tx) => {
      const [current] = await tx
        .select({ one: sql`1` })
        .from(reconcilerLeader)
        .where(eq(reconcilerLeader.epoch, epoch))
        .for("share");
      if (!current) throw new Superseded();
      return body(tx);
    });
}

async function reconcileEnvironment(run: Run, id: string) {
  const { deps, options, fenced, retries, signal } = run;
  const { db, logStore } = deps;
  const { provider } = options;
  // Read when the run starts, not when it was scheduled, since it may have waited for the pool.
  const [environment] = await db.select(environmentColumns).from(environments).where(eq(environments.id, id));
  if (!environment) return;
  const capacity = await db
    .select()
    .from(capacityRequests)
    .where(and(eq(capacityRequests.environment_id, id), not(capacityRequests.torn_down)))
    .orderBy(asc(capacityRequests.create_time));

  if (environment.state === EnvironmentState.DELETING) {
    // By name: a deleting environment's machine names are never used again. A failed removal holds up no other.
    const failures: unknown[] = [];
    for (const name of [...capacity.map((request) => capacityMachineName(request)), coreMachineName(id)]) {
      await provider.destroy(name).catch((error: unknown) => failures.push(error));
    }
    if (failures.length > 0) throw failures[0];
    // Once no machine is left to write them, and bounded like a provider call so a stalled store holds up no worker.
    if (logStore) {
      // Forks whose core hasn't attached yet still restore from this log, so it stays until they have or are deleted.
      const [restoring] = await db
        .select({ one: sql`1` })
        .from(environments)
        .where(
          and(
            eq(environments.forked_from_environment_id, id),
            eq(environments.epoch, 0n),
            ne(environments.state, EnvironmentState.DELETING),
          ),
        )
        .limit(1);
      if (restoring) return;
      const bound = AbortSignal.any([AbortSignal.timeout(options.timeouts.callMs), ...(signal ? [signal] : [])]);
      await untilAborted(bound, logStore.deleteEnvironment(id, bound));
    }
    await fenced((tx) =>
      tx.delete(environments).where(and(eq(environments.id, id), eq(environments.state, EnvironmentState.DELETING))),
    );
    for (const request of capacity) {
      retries.clear(capacityKey(request));
      retries.clear(teardownKey(request));
    }
    retries.clear(coreKey(id));
    return;
  }
  // A fork's core runs without a deployment: it restores the fork, and its first attach deploys the release the restored
  // data was serving, if any. Without one it suspends once idle, like any core.
  if (!environment.forked_from_environment_id && !(await desiredDeployment(db, id))) return;

  // Core has no state to fail into, so it retries for as long as it takes, backing off while calls fail transiently.
  const key = coreKey(id);
  if (retries.waiting(key)) return;
  try {
    await runCore(
      run,
      environment,
      capacity.filter(
        (request) => request.state === CapacityState.PROVISIONING || request.state === CapacityState.READY,
      ),
    );
    retries.clear(key);
  } catch (error) {
    if (!isTransient(error)) throw error;
    const overdue = retries.failed(key);
    (overdue ? console.error : console.warn)(`environment ${id}'s core machine is waiting: ${error.message}`);
  }
}

/** Runs core's machine, suspending it and the extra machines while idle, and the extra machines otherwise. */
async function runCore(run: Run, environment: EnvironmentRow, active: CapacityRow[]) {
  const { deps, options, fenced, retries } = run;
  const { db } = deps;
  const { provider } = options;
  const id = environment.id;
  const thisEnvironment = eq(environments.id, id);
  const observed = environment.machine_id ? await provider.status(environment.machine_id) : undefined;
  let core = observed && observed.state !== "missing" ? observed : await createCore(deps, options, fenced, environment);
  if (!core) return;

  const idle =
    environment.lease > 0n &&
    environment.ready_to_suspend &&
    environment.report_desired_revision === environment.revision;
  if (idle) {
    // Checked against the latest accepted report on every pass, retries included, since core may have reported
    // activity or accepted a wake after this pass read the environment. A later change resumes it next pass.
    const [marked] = await fenced((tx) =>
      tx
        .update(environments)
        .set({ state: EnvironmentState.SUSPENDED })
        .where(
          and(
            thisEnvironment,
            eq(environments.revision, environment.revision),
            gt(environments.lease, 0n),
            eq(environments.ready_to_suspend, true),
            eq(environments.report_desired_revision, environments.revision),
            ne(environments.state, EnvironmentState.DELETING),
          ),
        )
        .returning({ id: environments.id }),
    );
    if (!marked) return;
    if (environment.state !== EnvironmentState.SUSPENDED) await notify(db, { kind: "environment", environmentId: id });
    // Rechecked before each suspension, outside the transaction like every provider call. A report or wake that lands
    // between the recheck and the suspension notifies, which runs the environment again once this run ends, and that
    // run resumes the machine; so does a later one, for a suspension that finishes after it gave up on the call.
    const stillIdle = async () => {
      const [still] = await fenced((tx) =>
        tx
          .select({ idle: sql`1` })
          .from(environments)
          .where(
            and(
              thisEnvironment,
              eq(environments.state, EnvironmentState.SUSPENDED),
              eq(environments.revision, environment.revision),
              eq(environments.lease, environment.lease),
              eq(environments.ready_to_suspend, true),
              eq(environments.report_desired_revision, environments.revision),
            ),
          ),
      );
      return still !== undefined;
    };
    // A provider that stops machines rather than keeping their memory ends a JVM's session, so its request fails and
    // its machine is torn down while the environment sleeps; core asks for new capacity once woken. Judged from the
    // machine's state, not the suspension's reply, so one cut short is failed on a later pass.
    const failStopped = async (request: CapacityRow, machineId: string) => {
      await fenced((tx) =>
        tx
          .update(capacityRequests)
          .set({ state: CapacityState.FAILED, message: "the JVM machine stopped while its environment was suspended" })
          .where(
            and(
              eq(capacityRequests.environment_id, id),
              eq(capacityRequests.request_id, request.request_id),
              eq(capacityRequests.machine_id, machineId),
              inArray(capacityRequests.state, running),
            ),
          ),
      );
      await notify(db, { kind: "environment", environmentId: id });
    };
    // An extra machine whose calls fail is left for a later run, and the others and core are still suspended. Its
    // failures are kept until it is seen not running or its suspension succeeds.
    for (const request of active) {
      const machineId = request.machine_id;
      if (!machineId) continue;
      const key = capacityKey(request);
      const what = `suspending capacity ${request.request_id}`;
      const jvm = request.workload === Workload.JVM;
      const machine = await isolated(retries, key, what, () => provider.status(machineId));
      if (!machine) continue;
      if (machine.state !== "running") {
        retries.clear(key);
        if (jvm && request.started && machine.state === "stopped") await failStopped(request, machine.id);
        continue;
      }
      // Seen running, so its resume finished, and the next suspension may be resumed.
      if (request.resuming) {
        await fenced((tx) =>
          tx
            .update(capacityRequests)
            .set({ resuming: false })
            .where(
              and(
                eq(capacityRequests.environment_id, id),
                eq(capacityRequests.request_id, request.request_id),
                eq(capacityRequests.machine_id, machine.id),
              ),
            ),
        );
      }
      if (!(await stillIdle())) return;
      const suspended = await isolated(retries, key, what, () => provider.suspend(machine.id));
      if (!suspended) continue;
      retries.clear(key);
      if (jvm && suspended.state === "stopped") await failStopped(request, machine.id);
    }
    if (core.state === "running") {
      if (!(await stillIdle())) return;
      await provider.suspend(core.id);
    }
    await fireDueAlarm(fenced, environment);
    return;
  }

  if (core.state !== "running") {
    // A stopped core's gateways went with it, and no report of its lease is accepted after this; its successor attaches
    // under a new lease. The environment is starting from here, so the successor's first report with gateways runs it
    // without waiting for the start to return. A suspended core keeps its gateways, which serve again once it resumes.
    if (core.state === "stopped") {
      await fenced((tx) =>
        tx
          .update(environments)
          .set({
            gateway_addresses: [],
            report_sequence: closedSequence,
            state: sql`case
              when state in (${EnvironmentState.PENDING}, ${EnvironmentState.SUSPENDED}) then ${EnvironmentState.STARTING}::smallint
              else state
            end`,
          })
          .where(and(thisEnvironment, eq(environments.lease, environment.lease))),
      );
      await notify(db, { kind: "environment", environmentId: id });
    }
    core = await provider.start(core.id);
  }
  await saveCoreAddresses(deps, fenced, environment, core);
  if (environment.state === EnvironmentState.PENDING || environment.state === EnvironmentState.SUSPENDED) {
    // Core may report its gateways before its start returns, and such a report leaves the state as it is.
    await fenced((tx) =>
      tx
        .update(environments)
        .set({
          state: sql`case
            when cardinality(gateway_addresses) > 0 then ${EnvironmentState.RUNNING}::smallint
            else ${EnvironmentState.STARTING}::smallint
          end`,
        })
        .where(and(thisEnvironment, eq(environments.state, environment.state))),
    );
    await notify(db, { kind: "environment", environmentId: id });
  }
  for (const request of active) await keepRunning(run, core, request);
}

/** A report sequence no report exceeds, which closes a lease to further reports. */
const closedSequence = 2n ** 63n - 1n;

/** The states of a request whose machine should run. */
const running = [CapacityState.PROVISIONING, CapacityState.READY];

function coreKey(environmentId: string): string {
  return `core/${environmentId}`;
}

function capacityKey(request: Pick<CapacityRow, "environment_id" | "request_id">): string {
  return `capacity/${request.environment_id}/${request.request_id}`;
}

function teardownKey(request: Pick<CapacityRow, "environment_id" | "request_id">): string {
  return `teardown/${request.environment_id}/${request.request_id}`;
}

/**
 * Runs one extra machine's provider call so that its failure holds up no other machine: it is logged, and a later run
 * retries it once `key` has backed off. Resolves to undefined when the call failed or is backing off. The caller clears
 * `key` once the machine's step is done.
 */
async function isolated<T>(retries: RetryBackoff, key: string, what: string, call: () => Promise<T>) {
  if (retries.waiting(key)) return undefined;
  try {
    return await call();
  } catch (error) {
    const overdue = retries.failed(key);
    (isTransient(error) && !overdue ? console.warn : console.error)(`${what} failed, retrying later:`, error);
    return undefined;
  }
}

/**
 * Creates core's machine with the environment's token, issuing one first. Returns the machine once saved; one the
 * environment's deletion outran is destroyed, or swept when that fails.
 */
async function createCore(deps: Deps, options: ReconcilerOptions, fenced: Fence, environment: EnvironmentRow) {
  const { keys } = deps;
  const context = `machine-token/${environment.id}`;
  // The token is saved before the machine exists, so a retry after a crash builds the same machine, and in the same
  // transaction as the row lock deletion takes, so a deleted or deleting environment never gets a machine.
  const notDeleting = and(eq(environments.id, environment.id), ne(environments.state, EnvironmentState.DELETING));
  const sealed = await fenced(async (tx) => {
    const [row] = await tx
      .select({ machine_token: environments.machine_token })
      .from(environments)
      .where(notDeleting)
      .for("update");
    if (!row || row.machine_token) return row?.machine_token ?? undefined;
    const issued = await issueEnvironmentToken(tx, environment.id);
    const token = await keys.cipher.seal(new TextEncoder().encode(issued), context);
    await tx.update(environments).set({ machine_token: token }).where(eq(environments.id, environment.id));
    return token;
  });
  if (!sealed) return undefined;
  const token = new TextDecoder().decode(await keys.cipher.open(sealed, context));
  const machine = await options.provider.create(coreMachineSpec(options, environment.id, token));
  const saved = await fenced((tx) =>
    tx.update(environments).set({ machine_id: machine.id }).where(notDeleting).returning({ id: environments.id }),
  );
  if (saved.length === 0) {
    await options.provider.destroy(machine.name, { id: machine.id });
    return undefined;
  }
  return machine;
}

/** Addresses can change whenever a machine starts, and routes only list gateways on the current ones. */
async function saveCoreAddresses({ db }: Deps, fenced: Fence, environment: EnvironmentRow, core: Machine) {
  if (sameList(environment.machine_addresses, core.addresses)) return;
  await fenced((tx) =>
    tx.update(environments).set({ machine_addresses: core.addresses }).where(eq(environments.id, environment.id)),
  );
  await notify(db, { kind: "environment", environmentId: environment.id });
}

/**
 * Keeps an extra machine running and resumes a suspended one. A JVM machine boots at most once: its boot is recorded
 * before it starts, so a started one that stopped or went missing fails its request, and one never started is adopted
 * by name, or created again when missing. Each resume is recorded the same way until the machine is seen running, and
 * one never seen to finish fails its request rather than being resumed again, since a resume left under way could land
 * after the repeat and the JVM's exit, and boot it again. Failing tears the machine down, so a late start finds it gone
 * or is undone. Gateway machines are stateless and are replaced, with the same credential.
 * Machines are started by ID, and removed only under the ID observed, so a call a superseded pass left under way, or
 * one this leader gave up on, can neither start a destroyed machine nor remove its replacement. A transient provider
 * failure leaves the request as it is, and a later run retries with backoff; any other provider error, or transient
 * failures lasting `capacityRetryMs`, fail the request for good, and core retries with a new request ID.
 */
async function keepRunning(run: Run, core: Machine, request: CapacityRow) {
  const { deps, options, fenced, retries } = run;
  const { db, keys } = deps;
  const { provider } = options;
  const key = capacityKey(request);
  if (retries.waiting(key)) return;
  const jvm = request.workload === Workload.JVM;
  const name = capacityMachineName(request);
  const active = and(
    eq(capacityRequests.environment_id, request.environment_id),
    eq(capacityRequests.request_id, request.request_id),
    inArray(capacityRequests.state, running),
  );
  const notRunning = (machine: Machine | undefined) =>
    new Error(
      machine?.state === "suspended"
        ? "the JVM machine did not resume"
        : machine?.state === "stopped"
          ? "the JVM machine exited"
          : "the JVM machine went missing",
    );
  /** Whether a JVM machine in this state must not be started again. */
  const spent = (row: Pick<CapacityRow, "started" | "resuming">, machine: Machine | undefined) =>
    row.started && machine?.state !== "running" && (machine?.state !== "suspended" || row.resuming);
  let resuming = request.resuming;
  // Committed before the start, so a boot or resume cut short still counts as the one it was. A release after this
  // tears the machine down by name; a start still under way then finds its ID gone.
  const mayStart = (machine: Machine) =>
    fenced(async (tx) => {
      const [row] = await tx
        .select({ started: capacityRequests.started, resuming: capacityRequests.resuming })
        .from(capacityRequests)
        .where(and(active, eq(capacityRequests.machine_id, machine.id)))
        .for("update");
      if (!row) return false;
      if (jvm) {
        if (spent(row, machine)) throw notRunning(machine);
        resuming = machine.state === "suspended";
        await tx.update(capacityRequests).set({ started: true, resuming }).where(active);
      }
      return true;
    });
  try {
    let machine = request.machine_id ? await provider.status(request.machine_id) : undefined;
    if (jvm && spent(request, machine)) throw notRunning(machine);
    if (!machine || machine.state === "missing" || (!jvm && machine.state === "stopped")) {
      let found = await provider.find(name);
      // A stopped gateway is replaced: the one observed, or one left from an attempt whose create reply was lost.
      if (!jvm && found?.state === "stopped") {
        await provider.destroy(name, { id: found.id });
        found = undefined;
      }
      if (!found) {
        const coreHost = coreHostOf(core.addresses);
        if (!coreHost) return;
        const context = capacityCredentialContext(request.environment_id, request.request_id);
        const credential = new TextDecoder().decode(await keys.cipher.open(request.credential, context));
        found = await provider.create(capacityMachineSpec(options, request, { coreHost, credential }));
      }
      const id = found.id;
      const saved = await fenced((tx) =>
        tx
          .update(capacityRequests)
          .set({ machine_id: id })
          .where(active)
          .returning({ request_id: capacityRequests.request_id }),
      );
      if (saved.length === 0) {
        retries.clear(key);
        await provider.destroy(name, { id });
        return;
      }
      machine = found;
    }
    if (machine.state !== "running") {
      if (!(await mayStart(machine))) {
        retries.clear(key);
        return;
      }
      machine = await provider.start(machine.id);
    }
    retries.clear(key);
    if (request.state === CapacityState.READY && !resuming && sameList(request.machine_addresses, machine.addresses)) {
      return;
    }
    const addresses = machine.addresses;
    await fenced((tx) =>
      tx
        .update(capacityRequests)
        .set({ state: CapacityState.READY, machine_addresses: addresses, resuming: false })
        .where(active),
    );
  } catch (error) {
    if (error instanceof Superseded) throw error;
    if (isTransient(error) && !retries.failed(key)) {
      console.warn(`capacity ${request.request_id} is waiting: ${error.message}`);
      return;
    }
    retries.clear(key);
    console.error(`running capacity ${request.request_id} failed:`, error);
    await fenced((tx) =>
      tx
        .update(capacityRequests)
        .set({ state: CapacityState.FAILED, message: error instanceof Error ? error.message : String(error) })
        .where(active),
    );
  }
  await notify(db, { kind: "environment", environmentId: request.environment_id });
}

/** Tears down the machines of an environment's failed and releasing requests; a deleting one's run removes its own. */
async function tearDownReleased(run: Run, id: string) {
  const { db } = run.deps;
  const requests = await db
    .select()
    .from(capacityRequests)
    .where(
      and(
        eq(capacityRequests.environment_id, id),
        not(capacityRequests.torn_down),
        inArray(capacityRequests.state, [CapacityState.FAILED, CapacityState.RELEASING]),
        exists(
          db
            .select({ one: sql`1` })
            .from(environments)
            .where(and(eq(environments.id, id), ne(environments.state, EnvironmentState.DELETING))),
        ),
      ),
    )
    .orderBy(asc(capacityRequests.create_time));
  for (const request of requests) await tearDown(run, request);
}

/**
 * Removes a releasing or failed request's machine by name, which also finds one whose ID was never saved; the request
 * never runs a machine again, so nothing reuses the name. A releasing request is released once its machine is gone, and
 * a removal that fails is retried by a later run.
 */
async function tearDown({ options: { provider }, fenced, retries }: Run, request: CapacityRow) {
  retries.clear(capacityKey(request));
  const name = capacityMachineName(request);
  const what = `tearing down capacity ${request.request_id}`;
  const key = teardownKey(request);
  if (!(await isolated(retries, key, what, () => provider.destroy(name).then(() => true)))) return;
  retries.clear(key);
  await fenced((tx) =>
    tx
      .update(capacityRequests)
      .set({
        torn_down: true,
        state: sql`case when state = ${CapacityState.RELEASING}::smallint then ${CapacityState.RELEASED}::smallint else state end`,
      })
      .where(
        and(
          eq(capacityRequests.environment_id, request.environment_id),
          eq(capacityRequests.request_id, request.request_id),
        ),
      ),
  );
}

/**
 * Destroys the machines and volumes this install owns that nothing tracks: those of torn-down requests, and those whose
 * request or environment no longer exists. A create that finished, or was cut short, after its request was torn down or
 * its environment deleted leaves them. A request not yet torn down keeps its machine, since a create for it may still
 * be under way.
 */
async function sweep({ deps: { db }, options: { provider } }: Run) {
  const machines = await provider.list();
  // Read after listing: a request only ever becomes torn down and an environment only goes away, so a listed machine
  // untracked now stays untracked, and its name is never used again.
  const existing = await db.select({ id: environments.id }).from(environments);
  const requests = await db
    .select({
      environment_id: capacityRequests.environment_id,
      request_id: capacityRequests.request_id,
      workload: capacityRequests.workload,
    })
    .from(capacityRequests)
    .where(not(capacityRequests.torn_down));
  const tracked = new Set([
    ...existing.map(({ id }) => coreMachineName(id)),
    ...requests.map((request) => capacityMachineName(request)),
  ]);
  for (const { name } of machines) {
    if (tracked.has(name)) continue;
    try {
      await provider.destroy(name);
    } catch (error) {
      console.error(`removing untracked machine ${name} failed:`, error);
    }
  }
}

/**
 * Wakes a suspended environment once its alarm is due, by invalidating the idle report it suspended on. Only the
 * alarm read at the start of the pass is fired; a replacement stored since then is left for its own due time.
 */
async function fireDueAlarm(fenced: Fence, environment: EnvironmentRow) {
  const { alarm_due_seconds: seconds, alarm_due_nanos: nanos } = environment;
  if (seconds === null || environment.alarm_fired) return;
  if (Number(seconds) * 1000 + (nanos ?? 0) / 1e6 > Date.now()) return;
  await fenced(async (tx) => {
    const fired = await tx
      .update(environments)
      .set({ alarm_fired: true })
      .where(
        and(
          eq(environments.id, environment.id),
          not(environments.alarm_fired),
          eq(environments.alarm_epoch, environment.alarm_epoch),
          eq(environments.alarm_generation, environment.alarm_generation),
          eq(environments.alarm_due_seconds, seconds),
          sql`${environments.alarm_due_nanos} is not distinct from ${nanos}`,
        ),
      )
      .returning({ id: environments.id });
    if (fired.length > 0) await advanceRevision(tx, environment.id);
  });
}

function sameList(a: string[], b: string[]): boolean {
  return a.length === b.length && a.every((item, i) => item === b[i]);
}
