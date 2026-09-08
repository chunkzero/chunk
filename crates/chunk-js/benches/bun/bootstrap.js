(() => {
  "use strict";
  const read = __read, write = __write, harden = __harden;
  delete globalThis.__read;
  delete globalThis.__write;
  delete globalThis.__harden;
  // Best effort only: JavaScriptCore has no capability-free context. `Bun` is
  // non-configurable on worker and ShadowRealm globals, `import()` syntax and
  // prototype-chain escapes remain available to bundle code, and freezing a
  // vm context global breaks lazily materialized builtins in Bun 1.4.
  const retained = [];
  if (harden) for (const key of ["WebAssembly", "SharedArrayBuffer", "Atomics", "WeakRef",
    "FinalizationRegistry", "Intl", "Temporal", "Bun", "process", "require",
    "fetch", "WebSocket", "Worker", "setTimeout", "setInterval", "setImmediate",
    "queueMicrotask", "performance", "navigator"]) {
    try { delete globalThis[key]; } catch {}
    if (key in globalThis) retained.push(key);
  }
  const NativeDate = Date;
  let timestamp = 0, randomState = 1;
  class SnapshotDate extends NativeDate {
    constructor(...args) { super(...(args.length ? args : [timestamp])); }
    static now() { return timestamp; }
  }
  globalThis.Date = SnapshotDate;
  Math.random = () => {
    randomState ^= randomState << 13;
    randomState ^= randomState >>> 17;
    randomState ^= randomState << 5;
    return (randomState >>> 0) / 4294967296;
  };
  const db = Object.freeze({
    get: (table, id) => read({kind: "get", table, id}),
    scan: (table, start = null, end = null) => read({kind: "scan", table, start, end}),
    put: (table, id, value) => write({table, id}, value),
  });
  const stringify = JSON.stringify, finite = Number.isFinite;
  const serialize = value => stringify(value === undefined ? null : value, (_, item) => {
    if (typeof item === "undefined" || typeof item === "function" || typeof item === "symbol" ||
        (typeof item === "number" && !finite(item))) throw new Error("Result must be JSON");
    return item;
  });
  if (harden) Object.freeze(globalThis);
  return {
    context(caller, time, seed) {
      timestamp = time; randomState = seed || 1;
      return Object.freeze({caller, db});
    },
    serialize,
    retained,
  };
})()
