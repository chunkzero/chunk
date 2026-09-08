(() => {
  "use strict";
  const read = __read, write = __write;
  delete globalThis.__read;
  delete globalThis.__write;
  for (const key of ["WebAssembly", "SharedArrayBuffer", "Atomics", "WeakRef",
    "FinalizationRegistry", "Intl", "Temporal"]) delete globalThis[key];
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
  const parse = JSON.parse, stringify = JSON.stringify, finite = Number.isFinite;
  // Host values cross as JSON text and primitives; V8's JSON.parse builds objects.
  const db = Object.freeze({
    get: (table, id) => parse(read("get", table, id)),
    scan: (table, start = null, end = null) => parse(read("scan", table, start, end)),
    put: (table, id, value) => write(table, id, stringify(value)),
  });
  const serialize = value => stringify(value === undefined ? null : value, (_, item) => {
    if (typeof item === "undefined" || typeof item === "function" || typeof item === "symbol" ||
        (typeof item === "number" && !finite(item))) throw new Error("Result must be JSON");
    return item;
  });
  Object.freeze(globalThis);
  return {
    context(callerJson, time, seed) {
      timestamp = time; randomState = seed || 1;
      return Object.freeze({caller: parse(callerJson), db});
    },
    arguments: parse,
    serialize,
  };
})()
