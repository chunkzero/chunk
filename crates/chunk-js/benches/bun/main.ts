import { readFileSync } from "node:fs";
import { type Call, type Kind, bundleBytes, createEngine, type Outcome } from "./engine";
import { SyncEngine } from "./sync";
import { pad } from "./snapshot";

const WARMUP = Number(Bun.env.BENCH_WARMUP ?? 1000);
const CALLS = Number(Bun.env.BENCH_CALLS ?? 10000);
const WINDOW_MS = 80;
const INIT = (Bun.env.BENCH_INIT ?? "eager") as "eager" | "declarations";

const [engineName, exportName, kibText, burstText] = Bun.argv.slice(2);
if (!engineName || !exportName || !kibText) {
  console.error("usage: bun main.ts ENGINE empty|reads|query|sync BUNDLE_KIB [BURST_PER_80MS]");
  process.exit(2);
}
const kib = Number(kibText);
const burst = Number(burstText ?? 0);

const cpuUs = () => { const u = process.cpuUsage(); return u.user + u.system; };
const nowUs = () => Number(Bun.nanoseconds()) / 1e3;
const sleepUntil = async (us: number) => { const d = (us - nowUs()) / 1e3; if (d > 0) await Bun.sleep(d); };

function summary(values: number[]) {
  const sorted = [...values].sort((a, b) => a - b);
  return {
    mean: sorted.reduce((a, b) => a + b, 0) / sorted.length,
    median: sorted[Math.floor(sorted.length / 2)],
    p99: sorted[Math.min(Math.floor(sorted.length * 99 / 100), sorted.length - 1)],
    max: sorted[sorted.length - 1],
  };
}

function cpuDirectory(): [string, boolean] {
  const cgroup = readFileSync("/proc/self/cgroup", "utf8");
  for (const line of cgroup.split("\n")) {
    const [, controllers, path] = line.split(":");
    if (controllers?.split(",").includes("cpu")) return [`/sys/fs/cgroup/cpu,cpuacct${path}`, false];
  }
  const path = cgroup.split("\n").find((l) => l.startsWith("0::"))!.slice(3);
  return [`/sys/fs/cgroup${path}`, true];
}
const tryRead = (path: string) => { try { return readFileSync(path, "utf8"); } catch { return ""; } };
function cpuMax() {
  const [path, v2] = cpuDirectory();
  if (v2) return tryRead(`${path}/cpu.max`).trim();
  return `${tryRead(`${path}/cpu.cfs_quota_us`).trim()} ${tryRead(`${path}/cpu.cfs_period_us`).trim()}`;
}
const cpuStat = () => tryRead(`${cpuDirectory()[0]}/cpu.stat`);

type Sample = { value: string; phases: [number, number, number]; evaluations?: number };
type Runner = (i: number) => Promise<Sample> | Sample;

// Worker-hosted engines mirror the Rust harness: one dedicated thread per
// deployment, a channel round trip per call.
function workerEvaluate(kind: Kind): Promise<(call: Call) => Promise<Outcome>> {
  const worker = new Worker(new URL("./worker.ts", import.meta.url).href);
  const pending = new Map<number, { resolve: (o: Outcome) => void; reject: (e: Error) => void }>();
  let next = 0;
  return new Promise((ready) => {
    worker.onmessage = (event) => {
      const data = event.data as { ready?: boolean; id?: number; outcome?: Outcome; error?: string };
      if (data.ready) {
        ready((call) => new Promise((resolve, reject) => {
          const id = next++;
          // Like the Rust watchdog: a one-second deadline ends the worker outright.
          const deadline = setTimeout(() => { worker.terminate(); reject(new Error("deadline")); }, 1000);
          pending.set(id, { resolve: (o) => { clearTimeout(deadline); resolve(o); }, reject: (e) => { clearTimeout(deadline); reject(e); } });
          worker.postMessage({ call: { ...call, id } });
        }));
        return;
      }
      const entry = pending.get(data.id!)!;
      pending.delete(data.id!);
      if (data.error) entry.reject(new Error(data.error));
      else entry.resolve(data.outcome!);
    };
    worker.postMessage({ init: { kind, kib, init: INIT } });
  });
}

async function primitive(kind: string): Promise<Runner> {
  const vm = await import("node:vm");
  const { fixture, asScript } = await import("./fixture");
  const source = fixture(kib, INIT);
  const Realm = (globalThis as unknown as { ShadowRealm: new () => { evaluate(code: string): unknown } }).ShadowRealm;
  const workerSource = URL.createObjectURL(new Blob([`self.postMessage("ready")`], { type: "application/javascript" }));
  const loopSource = URL.createObjectURL(new Blob([`self.postMessage("entered"); while (true) {}`], { type: "application/javascript" }));
  const spawn = (url: string) => new Promise<Worker>((resolve) => {
    const worker = new Worker(url);
    worker.addEventListener("message", () => resolve(worker), { once: true });
  });
  const closed = (worker: Worker) => new Promise<void>((resolve) => worker.addEventListener("close", () => resolve(), { once: true }));
  const context = vm.createContext({});
  const SourceTextModule = (vm as unknown as { SourceTextModule: any }).SourceTextModule;
  const text = asScript(source);
  const produced = new vm.Script(text, { produceCachedData: true }).cachedData;
  const none = () => ({ value: "null", phases: [0, 0, 0] as [number, number, number] });
  switch (kind) {
    case "context":
      return () => { vm.createContext({}); return none(); };
    case "realm":
      return () => { new Realm().evaluate("1"); return none(); };
    case "worker":
      return async () => { const w = await spawn(workerSource); const c = closed(w); w.terminate(); await c; return none(); };
    case "cold":
      return async () => {
        // A fresh context with full ES-module compile/link/evaluate and no cache.
        const start = nowUs();
        const fresh = vm.createContext({ __read() { return null; }, __write() {} });
        const module = new SourceTextModule(source, { context: fresh });
        await module.link(() => { throw new Error("no imports"); });
        await module.evaluate();
        return { value: "null", phases: [0, 0, nowUs() - start] };
      };
    case "cached":
      return () => {
        const script = Bun.env.BENCH_CACHE === "none" ? new vm.Script(text) : new vm.Script(text, { cachedData: produced });
        if ((script as { cachedDataRejected?: boolean }).cachedDataRejected) throw new Error("cache rejected");
        script.runInContext(context);
        return none();
      };
    case "terminate":
      return async () => {
        const w = await spawn(loopSource);
        const start = nowUs();
        const c = closed(w);
        w.terminate();
        await c;
        return { value: "null", phases: [nowUs() - start, 0, 0] };
      };
    case "vmtimeout":
      return () => {
        // Synchronous loop terminated by the vm watchdog with the minimum 1 ms budget.
        let entered = 0;
        try {
          vm.runInNewContext("entered(); while (true) {}", { entered: () => { entered = nowUs(); } }, { timeout: 1 });
        } catch (error) {
          if (!String(error).includes("timed out")) throw error;
          return { value: "null", phases: [nowUs() - entered - 1000, 0, 0] };
        }
        throw new Error("loop did not time out");
      };
  }
  throw new Error(`unknown engine ${kind}`);
}

const expected = Array.from({ length: 7 }, (_, revision) => {
  switch (exportName) {
    case "reads": return JSON.stringify({ total: 45 + revision * 10 });
    case "query": return JSON.stringify(Array.from({ length: 10 }, (_, k) => ({ id: pad(19 - k), score: 19 - k + revision })));
    case "empty": return "null";
    default: return "";
  }
});

const call = (i: number): Call => ({ export: exportName, args: {}, revision: i, caller: { id: "benchmark-player" }, time: 1_700_000_000_000, seed: 42 });

async function build(): Promise<{ run: Runner; check: (i: number, sample: Sample) => void; finish?: () => Record<string, unknown> }> {
  const engines: Kind[] = ["inline", "persistent", "persistent-vm", "fresh-vm", "fresh-esm", "fresh-realm"];
  if (!engines.includes(engineName as Kind)) {
    if (exportName !== "empty") throw new Error("primitives use the empty export");
    return { run: await primitive(engineName), check: () => {} };
  }
  const evaluate = engineName === "inline"
    ? (await createEngine("inline", kib, INIT)).call
    : await workerEvaluate(engineName as Kind);
  if (exportName === "sync") {
    const sync = new SyncEngine(evaluate);
    await sync.subscribe();
    let operations = 0;
    return {
      run: async (i) => {
        const before = sync.evaluations;
        const { value, reevaluated } = await sync.operate(i, pad(i % 20));
        operations += 1;
        return { value, phases: [reevaluated, 0, 0], evaluations: sync.evaluations - before };
      },
      // Mutation n on player p returns p + 1 + previous bumps of p.
      check: (i, sample) => {
        const p = i % 20;
        const bumps = Math.floor(i / 20);
        if (sample.value !== String(p + 1 + bumps)) throw new Error(`op ${i}: ${sample.value}`);
        if (sample.phases[0] !== 2) throw new Error(`op ${i}: ${sample.phases[0]} re-evaluations`);
      },
      finish: () => {
        const final = sync.subscriptions[0].result;
        if (final !== sync.leaderboard()) throw new Error(`subscription drifted: ${final} vs ${sync.leaderboard()}`);
        return { operations, evaluations: sync.evaluations, published: sync.published, revision: sync.revision, leaderboard: final };
      },
    };
  }
  return {
    run: async (i) => { const o = await evaluate(call(i)); if (o.writes.length) throw new Error("unexpected writes"); return { value: o.value, phases: o.phases }; },
    check: (i, sample) => { if (sample.value !== expected[i % 7]) throw new Error(`call ${i}: ${sample.value} != ${expected[i % 7]}`); },
  };
}

const { run, check, finish } = await build();
for (let i = 0; i < WARMUP; i++) check(i, await run(i));
const syncStateReset = exportName === "sync";
const wall: number[] = [];
const cpu: number[] = [];
const response: number[] = [];
const phases: [number[], number[], number[]] = [[], [], []];
const windows: number[] = [];
const statsBefore = cpuStat();
const cpuStart = cpuUs();
const begin = nowUs();
let windowCpu = cpuStart;
for (let i = 0; i < CALLS; i++) {
  const index = WARMUP + i;
  const scheduled = burst > 0 ? begin + WINDOW_MS * 1e3 * Math.floor(i / burst) : nowUs();
  if (burst > 0 && i % burst === 0) {
    if (i > 0) windows.push(cpuUs() - windowCpu);
    await sleepUntil(scheduled);
    windowCpu = cpuUs();
  }
  const start = nowUs();
  const cpuBefore = cpuUs();
  const sample = await run(syncStateReset ? index : i);
  cpu.push(cpuUs() - cpuBefore);
  wall.push(nowUs() - start);
  response.push(nowUs() - scheduled);
  check(syncStateReset ? index : i, sample);
  sample.phases.forEach((value, k) => phases[k].push(value));
}
if (burst > 0) windows.push(cpuUs() - windowCpu);
const elapsed = (nowUs() - begin) / 1e6;
const processCpu = cpuUs() - cpuStart;
const status = readFileSync("/proc/self/status", "utf8");
console.log(JSON.stringify({
  engine: `bun-${engineName}`, export: exportName, bundle_bytes: bundleBytes(kib, INIT),
  cache_stage: Bun.env.BENCH_CACHE ?? "instantiate", bundle_init: INIT,
  runtime: `bun ${Bun.version}`, calls: CALLS, warmup: WARMUP, burst_per_80ms: burst,
  cpu_max: cpuMax(), cpu_stat_before: statsBefore, cpu_stat_after: cpuStat(),
  wall_us: summary(wall), call_cpu_us: summary(cpu), response_us: summary(response),
  process_cpu_us_per_call: processCpu / CALLS, elapsed_seconds: elapsed, calls_per_second: CALLS / elapsed,
  context_us: summary(phases[0]), bootstrap_us: summary(phases[1]), module_us: summary(phases[2]),
  termination_us: engineName === "terminate" || engineName === "vmtimeout" ? summary(phases[0]) : null,
  batch_cpu_us: windows.length ? summary(windows) : null,
  sync: finish?.() ?? null,
  peak_rss: status.split("\n").find((l) => l.startsWith("VmHWM:")) ?? "",
}));
process.exit(0);
