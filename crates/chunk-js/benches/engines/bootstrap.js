// The transactional part of chunk-js's bootstrap, with host functions instead of deno_core ops.
(() => {
  "use strict";
  const read = globalThis.chunkRead;
  const write = globalThis.chunkWrite;
  const now = globalThis.chunkNow;
  const random = globalThis.chunkRandom;
  for (const name of ["chunkRead", "chunkWrite", "chunkNow", "chunkRandom"]) delete globalThis[name];
  const freeze = Object.freeze;
  const parse = JSON.parse;
  const stringify = JSON.stringify;
  const finite = Number.isFinite;
  const serialize = (value) =>
    stringify(value === undefined ? null : value, (_, item) => {
      if (
        typeof item === "undefined" ||
        typeof item === "function" ||
        typeof item === "symbol" ||
        (typeof item === "number" && !finite(item))
      )
        throw new Error("Result must be JSON");
      return item;
    });
  const unavailable = () => {
    throw new Error("Scheduling is unavailable in this benchmark");
  };
  const construct = Reflect.construct;
  const NativeDate = Date;
  const dateString = Function.prototype.call.bind(NativeDate.prototype.toString);
  const controlledDate = new Proxy(NativeDate, {
    apply: () => dateString(new NativeDate(now())),
    construct: (target, args, newTarget) => construct(target, args.length ? args : [now()], newTarget),
  });
  Object.defineProperty(NativeDate, "now", { value: now, writable: false, configurable: false });
  globalThis.Date = controlledDate;
  Object.defineProperty(Math, "random", { value: random, writable: false, configurable: false });
  return async (handler, callerJson, generation, argsJson) => {
    const context = freeze({
      caller: parse(callerJson),
      scheduler: freeze({ runAt: unavailable, cancel: unavailable, retry: unavailable }),
      db: freeze({
        get: (table, id) => parse(read(generation, stringify({ kind: "get", table, id }))),
        scan: (table, start = null, end = null) =>
          parse(read(generation, stringify({ kind: "scan", table, start, end }))),
        scanIndex: (query) => parse(read(generation, stringify({ kind: "index", query }))),
        put: (table, id, value) => write(generation, stringify({ kind: "put", key: { table, id }, value })),
        delete: (table, id) => write(generation, stringify({ kind: "delete", key: { table, id } })),
      }),
    });
    return serialize(await handler(context, parse(argsJson)));
  };
})();
