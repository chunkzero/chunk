(() => {
  "use strict";
  const schedule = Deno.core.ops.op_chunk_schedule;
  const action = Deno.core.ops.op_chunk_action;
  const actionId = Deno.core.ops.op_chunk_action_id;
  const read = Deno.core.ops.op_chunk_read;
  const write = Deno.core.ops.op_chunk_write;
  const now = Deno.core.ops.op_chunk_now;
  const random = Deno.core.ops.op_chunk_random;
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
    throw new Error("API unavailable in transactional execution");
  };
  const construct = Reflect.construct;
  const NativeDate = Date;
  const dateString = Function.prototype.call.bind(NativeDate.prototype.toString);
  const controlledDate = new Proxy(NativeDate, {
    apply: () => dateString(new NativeDate(now())),
    construct: (target, args, newTarget) => construct(target, args.length ? args : [now()], newTarget),
  });
  Object.defineProperty(NativeDate, "now", { value: now, writable: false, configurable: false });
  Object.defineProperty(NativeDate.prototype, "constructor", {
    value: controlledDate,
    writable: false,
    configurable: false,
  });
  globalThis.Date = controlledDate;
  Object.defineProperty(Math, "random", { value: random, writable: false, configurable: false });
  const boundedBuffer = new Proxy(ArrayBuffer, {
    construct: (target, args, newTarget) => {
      if (args[1]?.maxByteLength !== undefined) throw new Error("Resizable buffers unavailable");
      return construct(target, args.length ? [args[0]] : [], newTarget);
    },
  });
  Object.defineProperty(ArrayBuffer.prototype, "constructor", {
    value: boundedBuffer,
    writable: false,
    configurable: false,
  });
  globalThis.ArrayBuffer = boundedBuffer;
  for (const name of [
    "Deno",
    "__bootstrap",
    "__infra",
    "Temporal",
    "Intl",
    "performance",
    "WeakRef",
    "FinalizationRegistry",
    "WebAssembly",
    "SharedArrayBuffer",
  ]) {
    delete globalThis[name];
  }
  for (const prototype of [String.prototype, Number.prototype, BigInt.prototype, Array.prototype]) {
    for (const name of ["localeCompare", "toLocaleString", "toLocaleLowerCase", "toLocaleUpperCase"]) {
      if (name in prototype)
        Object.defineProperty(prototype, name, { value: unavailable, writable: false, configurable: false });
    }
  }
  return async (handler, callerJson, generation, argsJson) => {
    const invocationId = actionId();
    const context =
      typeof invocationId === "string"
        ? freeze({
            caller: parse(callerJson),
            invocationId,
            http: async (binding, request) =>
              parse(await action(stringify({ kind: "http", request: { ...request, binding } }))),
            secret: async (name) => parse(await action(stringify({ kind: "secret", name }))),
            platform: async (request) => parse(await action(stringify({ kind: "platform", request }))),
            runQuery: async (functionPath, argumentsValue) =>
              parse(await action(stringify({ kind: "query", function: functionPath, arguments: argumentsValue }))),
            runMutation: async (functionPath, argumentsValue) =>
              parse(await action(stringify({ kind: "mutation", function: functionPath, arguments: argumentsValue }))),
            sleep: async (milliseconds) => {
              if (!Number.isSafeInteger(milliseconds) || milliseconds < 0) throw new Error("Invalid sleep duration");
              await action(stringify({ kind: "sleep", milliseconds }));
            },
          })
        : freeze({
            caller: parse(callerJson),
            scheduler: freeze({
              runAt: (at, functionPath, argumentsValue) =>
                parse(
                  schedule(
                    generation,
                    stringify({ kind: "run_at", at, function: functionPath, arguments: argumentsValue }),
                  ),
                ),
              cancel: (id) => {
                schedule(generation, stringify({ kind: "cancel", id }));
              },
              retry: (id, at, acknowledgePossibleEffects) => {
                schedule(
                  generation,
                  stringify({ kind: "retry", id, at, acknowledge_possible_effects: acknowledgePossibleEffects }),
                );
              },
            }),
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
