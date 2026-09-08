import { drainMicrotasks } from "bun:jsc";
import vm from "node:vm";
import bootstrapText from "./bootstrap.js" with { type: "text" };
import { asScript, fixture } from "./fixture";
import { type Deps, type Rows, Snapshot, type Write, revisionRows } from "./snapshot";

export type Kind = "inline" | "persistent" | "persistent-vm" | "fresh-vm" | "fresh-esm" | "fresh-realm";
export type Call = {
  export: string;
  args: unknown;
  revision?: number;
  rows?: Rows;
  caller: unknown;
  time: number;
  seed: number;
};
export type Outcome = { value: string; deps: Deps; writes: Write[]; phases: [number, number, number] };
export type Engine = { call(call: Call): Outcome };

type Bootstrap = {
  context(caller: unknown, time: number, seed: number): unknown;
  serialize(value: unknown): string;
  retained: string[];
};
type Namespace = Record<string, (ctx: unknown, args: unknown) => unknown>;
type Loaded = { bootstrap: Bootstrap; namespace: Namespace };

const now = performance.now.bind(performance);
const CACHE = Bun.env.BENCH_CACHE ?? "instantiate";

// Runs the loaded handler with a fresh capability set and settles its promise
// through an explicit microtask checkpoint, like the V8 harness.
function invoke(loaded: Loaded, snapshot: Snapshot, call: Call): string {
  const ctx = loaded.bootstrap.context(call.caller, call.time, call.seed);
  const handler = loaded.namespace[call.export];
  if (!handler) throw new Error(`unknown export ${call.export}`);
  let value = handler(ctx, call.args);
  if (value && typeof (value as Promise<unknown>).then === "function") {
    let settled = false;
    let error: unknown;
    (value as Promise<unknown>).then(
      (v) => { settled = true; value = v; },
      (e) => { settled = true; error = e ?? new Error("rejected"); },
    );
    drainMicrotasks();
    if (!settled) throw new Error("promise still pending after microtask drain");
    if (error) throw error;
  }
  const text = loaded.bootstrap.serialize(value);
  if (text.length > 1024 * 1024) throw new Error("result too large");
  return text;
}

function outcome(loaded: Loaded, snapshot: Snapshot, call: Call, phases: [number, number, number]): Outcome {
  const value = invoke(loaded, snapshot, call);
  return { value, deps: snapshot.deps(), writes: snapshot.writes, phases };
}

function rowsFor(call: Call): Rows {
  if (call.rows) return call.rows;
  if (call.revision === undefined) throw new Error("call needs rows or revision");
  return revisionRows(call.revision);
}

// A mutable slot lets a once-created global/context route host calls to the
// snapshot of the current invocation, like the isolate slot in the Rust harness.
type Slot = { current: Snapshot | null };
function hostFunctions(slot: Slot, harden: boolean) {
  return {
    __read: (read: Parameters<Snapshot["read"]>[0]) => slot.current!.read(read),
    __write: (key: Write["key"], value: Write["value"]) => slot.current!.write(key, value),
    __harden: harden,
  };
}

async function importOnce(source: string): Promise<Namespace> {
  const url = URL.createObjectURL(new Blob([source], { type: "application/javascript" }));
  return (await import(url)) as Namespace;
}

// The inline engine shares the harness global object, so it keeps ambient
// globals and only installs deterministic time/random and the capability API.
function globalEngine(source: string, harden: boolean): Promise<Engine> {
  const slot: Slot = { current: null };
  return importOnce(source).then((namespace) => {
    Object.assign(globalThis, hostFunctions(slot, harden));
    const bootstrap = (0, eval)(bootstrapText) as Bootstrap;
    const loaded = { bootstrap, namespace };
    return {
      call(call) {
        slot.current = new Snapshot(rowsFor(call));
        return outcome(loaded, slot.current, call, [0, 0, 0]);
      },
    };
  });
}

function vmScripts(source: string) {
  const bootstrapScript = new vm.Script(bootstrapText, { filename: "chunk:bootstrap" });
  const text = asScript(source);
  let moduleScript = new vm.Script(text, { filename: "chunk:deployment/benchmark", produceCachedData: true });
  if (CACHE !== "none") {
    const cachedData = moduleScript.cachedData;
    if (!cachedData) throw new Error("no cached data produced");
    moduleScript = new vm.Script(text, { filename: "chunk:deployment/benchmark", cachedData });
    if (moduleScript.cachedDataRejected) throw new Error("code cache rejected");
  }
  return { bootstrapScript, moduleScript };
}

function loadVm(scripts: ReturnType<typeof vmScripts>, slot: Slot): { loaded: Loaded; phases: [number, number, number] } {
  const start = now();
  const context = vm.createContext(hostFunctions(slot, false));
  const contextUs = (now() - start) * 1e3;
  const b = now();
  const bootstrap = scripts.bootstrapScript.runInContext(context) as Bootstrap;
  const bootstrapUs = (now() - b) * 1e3;
  const m = now();
  const namespace = scripts.moduleScript.runInContext(context) as Namespace;
  const moduleUs = (now() - m) * 1e3;
  return { loaded: { bootstrap, namespace }, phases: [contextUs, bootstrapUs, moduleUs] };
}

function vmEngine(source: string, persistent: boolean): Engine {
  const slot: Slot = { current: null };
  const scripts = vmScripts(source);
  const initial = persistent ? loadVm(scripts, slot).loaded : null;
  return {
    call(call) {
      slot.current = new Snapshot(rowsFor(call));
      if (initial) return outcome(initial, slot.current, call, [0, 0, 0]);
      const { loaded, phases } = loadVm(scripts, slot);
      return outcome(loaded, slot.current, call, phases);
    },
  };
}

// ES module semantics inside a vm context: compiled, linked and evaluated per call.
function esmEngine(source: string): Engine {
  const slot: Slot = { current: null };
  const bootstrapScript = new vm.Script(bootstrapText, { filename: "chunk:bootstrap" });
  const SourceTextModule = (vm as unknown as { SourceTextModule: any }).SourceTextModule;
  return {
    call(call) {
      slot.current = new Snapshot(rowsFor(call));
      const start = now();
      const context = vm.createContext(hostFunctions(slot, false));
      const contextUs = (now() - start) * 1e3;
      const b = now();
      const bootstrap = bootstrapScript.runInContext(context) as Bootstrap;
      const bootstrapUs = (now() - b) * 1e3;
      const m = now();
      const module = new SourceTextModule(source, { context, identifier: "chunk:deployment/benchmark" });
      let linked = false;
      let evaluated = false;
      module.link(() => { throw new Error("imports require bundling"); }).then(() => { linked = true; });
      drainMicrotasks();
      if (!linked) throw new Error("link pending");
      module.evaluate().then(() => { evaluated = true; });
      drainMicrotasks();
      if (!evaluated || module.status !== "evaluated") throw new Error(`module ${module.status}`);
      const moduleUs = (now() - m) * 1e3;
      return outcome({ bootstrap, namespace: module.namespace }, slot.current, call, [contextUs, bootstrapUs, moduleUs]);
    },
  };
}

// Only primitives and callables cross a ShadowRealm boundary, so host reads,
// writes, arguments and results travel as JSON text.
function realmEngine(source: string): Engine {
  const Realm = (globalThis as unknown as { ShadowRealm: new () => { evaluate(code: string): any } }).ShadowRealm;
  const slot: Slot = { current: null };
  const hostRead = (json: string) => JSON.stringify(slot.current!.read(JSON.parse(json)));
  const hostWrite = (json: string) => { const [key, value] = JSON.parse(json); slot.current!.write(key, value); };
  const install = `(readJson, writeJson) => {
    globalThis.__read = request => JSON.parse(readJson(JSON.stringify(request)));
    globalThis.__write = (key, value) => { writeJson(JSON.stringify([key, value])); };
    globalThis.__harden = true;
  }`;
  const driver = `(() => {
    const bootstrap = ${bootstrapText};
    const namespace = ${asScript(source)};
    let out, done = false, failure;
    return (name, callerJson, time, seed, argsJson) => {
      if (name === "\\0take") { if (failure) throw failure; if (!done) throw new Error("promise still pending"); return out; }
      done = false; out = undefined; failure = undefined;
      const ctx = bootstrap.context(JSON.parse(callerJson), time, seed);
      const value = namespace[name](ctx, JSON.parse(argsJson));
      if (value && typeof value.then === "function") {
        value.then(v => { out = bootstrap.serialize(v); done = true; }, e => { failure = e; done = true; });
        return undefined;
      }
      done = true;
      return bootstrap.serialize(value);
    };
  })()`;
  return {
    call(call) {
      slot.current = new Snapshot(rowsFor(call));
      const start = now();
      const realm = new Realm();
      const contextUs = (now() - start) * 1e3;
      const b = now();
      realm.evaluate(install)(hostRead, hostWrite);
      const bootstrapUs = (now() - b) * 1e3;
      const m = now();
      const run = realm.evaluate(driver) as (...a: unknown[]) => string | undefined;
      const moduleUs = (now() - m) * 1e3;
      let value = run(call.export, JSON.stringify(call.caller), call.time, call.seed, JSON.stringify(call.args));
      if (value === undefined) {
        drainMicrotasks();
        value = run("\0take");
      }
      if (typeof value !== "string" || value.length > 1024 * 1024) throw new Error("bad result");
      return { value, deps: slot.current.deps(), writes: slot.current.writes, phases: [contextUs, bootstrapUs, moduleUs] };
    },
  };
}

export async function createEngine(kind: Kind, kib: number, init: "eager" | "declarations"): Promise<Engine> {
  const source = fixture(kib, init);
  switch (kind) {
    case "inline": return globalEngine(source, false);
    case "persistent": return globalEngine(source, true);
    case "persistent-vm": return vmEngine(source, true);
    case "fresh-vm": return vmEngine(source, false);
    case "fresh-esm": return esmEngine(source);
    case "fresh-realm": return realmEngine(source);
  }
}

export function bundleBytes(kib: number, init: "eager" | "declarations") {
  return Buffer.byteLength(fixture(kib, init));
}
