// A minimal single-writer sync engine: serialized mutation commit with read-set
// validation, then invalidation and re-evaluation of subscribed queries.
import type { Call, Outcome } from "./engine";
import { type Deps, type Doc, type Rows, type Write, pad } from "./snapshot";

export type Evaluate = (call: Call) => Promise<Outcome> | Outcome;

type Subscription = { args: { start: string; end: string }; deps: Deps; result: string };

export const SUBSCRIPTIONS = [
  { start: "0000", end: "0020" },
  { start: "0000", end: "0005" },
  { start: "0005", end: "0010" },
  { start: "0010", end: "0015" },
  { start: "0015", end: "0020" },
];

const CALLER = { id: "benchmark-player" };
const TIME = 1_700_000_000_000;
const SEED = 42;

export class SyncEngine {
  rows: Rows = {};
  rowRevision = new Map<string, number>();
  revision = 0;
  subscriptions: Subscription[] = [];
  evaluations = 0;
  published = 0;
  outcomes = new Map<number, string>();

  constructor(private evaluate: Evaluate) {
    for (let i = 0; i < 20; i++) this.rows[pad(i)] = { score: i, name: `player-${i}`, online: true };
  }

  private snapshotRows(): Rows {
    const rows: Rows = {};
    for (const [id, doc] of Object.entries(this.rows)) rows[id] = { ...doc };
    return rows;
  }

  private async call(name: string, args: unknown): Promise<Outcome> {
    this.evaluations += 1;
    return this.evaluate({ export: name, args, rows: this.snapshotRows(), caller: CALLER, time: TIME, seed: SEED });
  }

  async subscribe() {
    for (const args of SUBSCRIPTIONS) {
      const outcome = await this.call("query", args);
      this.subscriptions.push({ args, deps: outcome.deps, result: outcome.value });
    }
  }

  private validate(deps: Deps, snapshot: number) {
    for (const id of deps.points) if ((this.rowRevision.get(id) ?? 0) > snapshot) throw new Error("conflict");
    for (const [, start, end] of deps.scans) {
      for (const [id, revision] of this.rowRevision) {
        if ((start === null || id >= start) && (end === null || id < end) && revision > snapshot) throw new Error("conflict");
      }
    }
  }

  private affected(deps: Deps, writes: Write[]) {
    return writes.some(({ key }) => deps.points.includes(key.id)
      || deps.scans.some(([, start, end]) => (start === null || key.id >= start) && (end === null || key.id < end)));
  }

  // One operation: speculative mutation, validation, atomic apply, invalidation.
  async operate(operation: number, id: string): Promise<{ value: string; reevaluated: number }> {
    const snapshot = this.revision;
    const outcome = await this.call("bump", { id });
    this.validate(outcome.deps, snapshot);
    this.revision += 1;
    for (const { key, value } of outcome.writes) {
      if (value === null) delete this.rows[key.id];
      else this.rows[key.id] = value as Doc;
      this.rowRevision.set(key.id, this.revision);
    }
    this.outcomes.set(operation, outcome.value);
    let reevaluated = 0;
    for (const subscription of this.subscriptions) {
      if (!this.affected(subscription.deps, outcome.writes)) continue;
      reevaluated += 1;
      const fresh = await this.call("query", subscription.args);
      subscription.deps = fresh.deps;
      if (fresh.value !== subscription.result) this.published += 1;
      subscription.result = fresh.value;
    }
    return { value: outcome.value, reevaluated };
  }

  leaderboard(): string {
    return JSON.stringify(Object.entries(this.rows)
      .map(([id, doc]) => ({ id, score: doc.score }))
      .sort((a, b) => b.score - a.score || (a.id < b.id ? -1 : 1))
      .slice(0, 10));
  }
}
