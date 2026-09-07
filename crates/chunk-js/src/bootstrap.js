(() => {
  "use strict";
  const read = Deno.core.ops.op_chunk_read;
  const write = Deno.core.ops.op_chunk_write;
  const freeze = Object.freeze;
  const parse = JSON.parse;
  const stringify = JSON.stringify;
  const unavailable = () => { throw new Error("API unavailable in transactional execution"); };
  Object.defineProperty(Math, "random", { value: unavailable, writable: false, configurable: false });
  for (const name of ["Deno", "__bootstrap", "__infra", "console", "Date", "Temporal", "Intl", "performance", "WeakRef", "FinalizationRegistry", "WebAssembly", "SharedArrayBuffer", "ArrayBuffer", "DataView", "Int8Array", "Uint8Array", "Uint8ClampedArray", "Int16Array", "Uint16Array", "Int32Array", "Uint32Array", "Float16Array", "Float32Array", "Float64Array", "BigInt64Array", "BigUint64Array"]) {
    delete globalThis[name];
  }
  for (const prototype of [String.prototype, Number.prototype, BigInt.prototype, Array.prototype]) {
    for (const name of ["localeCompare", "toLocaleString", "toLocaleLowerCase", "toLocaleUpperCase"]) {
      if (name in prototype) Object.defineProperty(prototype, name, { value: unavailable, writable: false, configurable: false });
    }
  }
  return (caller) => freeze({
    caller,
    db: freeze({
      get: (table, id) => parse(read(stringify({kind: "get", table, id}))),
      scan: (table, start = null, end = null) => parse(read(stringify({kind: "scan", table, start, end}))),
      put: (table, id, value) => write(stringify({kind: "put", key: {table, id}, value})),
      delete: (table, id) => write(stringify({kind: "delete", key: {table, id}})),
    }),
  });
})()
