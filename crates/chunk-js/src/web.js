(() => {
  "use strict";
  const core = Deno.core;
  const encoding = core.loadExtScript("ext:deno_web/08_text_encoding.js");
  const url = core.loadExtScript("ext:deno_web/00_url.js");
  const base64 = core.loadExtScript("ext:deno_web/05_base64.js");
  const { DOMException } = core.loadExtScript("ext:deno_web/01_dom_exception.js");
  const random = core.ops.op_chunk_random;
  const log = core.ops.op_chunk_log;
  const digest = core.ops.op_chunk_digest;
  const Uint8 = Uint8Array;
  // A JavaScript clone, since V8's serializer runs host-object hooks keyed by
  // the global Symbol.for("Deno.core.hostObject") brand. It accepts an explicit
  // list of types and throws DataCloneError for anything else. Built-ins are
  // found by internal slot and read through methods captured before application
  // code runs, so modified prototypes cannot change or observe the copy.
  const clone = (() => {
    const ops = core.ops;
    const uncurry = (fn) => Function.prototype.call.bind(fn);
    const getter = (prototype, name) => uncurry(Object.getOwnPropertyDescriptor(prototype, name).get);
    const { defineProperty, getOwnPropertyDescriptor, getPrototypeOf, ownKeys } = Reflect;
    const { isArray } = Array;
    const { isRawJSON } = JSON;
    const [ObjectPrototype, ArrayPrototype] = [Object.prototype, Array.prototype];
    const [NativeObject, NativeMap, NativeSet, NativeDate, NativeRegExp] = [Object, Map, Set, Date, RegExp];
    const [NativeArrayBuffer, NativeDataView] = [ArrayBuffer, DataView];
    const mapGet = uncurry(Map.prototype.get);
    const mapSet = uncurry(Map.prototype.set);
    const mapHas = uncurry(Map.prototype.has);
    const mapEach = uncurry(Map.prototype.forEach);
    const setAdd = uncurry(Set.prototype.add);
    const setEach = uncurry(Set.prototype.forEach);
    const time = uncurry(Date.prototype.getTime);
    const source = getter(RegExp.prototype, "source");
    const flags = [
      ["d", "hasIndices"],
      ["g", "global"],
      ["i", "ignoreCase"],
      ["m", "multiline"],
      ["s", "dotAll"],
      ["u", "unicode"],
      ["v", "unicodeSets"],
      ["y", "sticky"],
    ].map(([flag, name]) => [flag, getter(RegExp.prototype, name)]);
    const bufferLength = getter(ArrayBuffer.prototype, "byteLength");
    const detached = getter(ArrayBuffer.prototype, "detached");
    const TypedArray = getPrototypeOf(Uint8).prototype;
    const viewName = getter(TypedArray, Symbol.toStringTag);
    const viewBuffer = getter(TypedArray, "buffer");
    const viewOffset = getter(TypedArray, "byteOffset");
    const viewLength = getter(TypedArray, "length");
    const viewSet = uncurry(TypedArray.set);
    const dataBuffer = getter(DataView.prototype, "buffer");
    const dataOffset = getter(DataView.prototype, "byteOffset");
    const dataLength = getter(DataView.prototype, "byteLength");
    const boxed = [
      [ops.op_is_number_object, uncurry(Number.prototype.valueOf)],
      [ops.op_is_string_object, uncurry(String.prototype.valueOf)],
      [ops.op_is_boolean_object, uncurry(Boolean.prototype.valueOf)],
      [ops.op_is_big_int_object, uncurry(BigInt.prototype.valueOf)],
    ];
    // Exotic objects that can still have an ordinary prototype after setPrototypeOf.
    const exotic = [
      ops.op_is_arguments_object,
      ops.op_is_generator_object,
      ops.op_is_map_iterator,
      ops.op_is_module_namespace_object,
      ops.op_is_native_error,
      ops.op_is_promise,
      ops.op_is_set_iterator,
      ops.op_is_shared_array_buffer,
      ops.op_is_symbol_object,
      ops.op_is_weak_map,
      ops.op_is_weak_set,
      isRawJSON,
    ];
    const views = { __proto__: null };
    for (const View of [
      Int8Array,
      Uint8Array,
      Uint8ClampedArray,
      Int16Array,
      Uint16Array,
      Int32Array,
      Uint32Array,
      Float16Array,
      Float32Array,
      Float64Array,
      BigInt64Array,
      BigUint64Array,
    ])
      views[View.name] = View;
    const fail = (kind) => {
      throw new DOMException(`${kind} could not be cloned`, "DataCloneError");
    };
    // A getter may detach a buffer after it was copied, so check it before reusing the copy.
    const copyViewBuffer = (buffer, memory) => {
      if (detached(buffer)) fail("Detached ArrayBuffer");
      return copy(buffer, memory);
    };
    const copy = (value, memory) => {
      if (typeof value === "function") fail("Function");
      if (typeof value === "symbol") fail("Symbol");
      if (typeof value !== "object" || value === null) return value;
      if (mapHas(memory, value)) return mapGet(memory, value);
      const remember = (result) => {
        mapSet(memory, value, result);
        return result;
      };
      if (ops.op_is_proxy(value)) fail("Proxy");
      if (ops.op_is_date(value)) return remember(new NativeDate(time(value)));
      if (ops.op_is_reg_exp(value)) {
        let present = "";
        for (let i = 0; i < flags.length; i++) if (flags[i][1](value)) present += flags[i][0];
        return remember(new NativeRegExp(source(value), present));
      }
      if (ops.op_is_array_buffer(value)) {
        if (detached(value)) fail("Detached ArrayBuffer");
        const result = new NativeArrayBuffer(bufferLength(value));
        viewSet(new Uint8(result), new Uint8(value));
        return remember(result);
      }
      if (ops.op_is_typed_array(value)) {
        const buffer = copyViewBuffer(viewBuffer(value), memory);
        return remember(new views[viewName(value)](buffer, viewOffset(value), viewLength(value)));
      }
      if (ops.op_is_data_view(value)) {
        const buffer = copyViewBuffer(dataBuffer(value), memory);
        return remember(new NativeDataView(buffer, dataOffset(value), dataLength(value)));
      }
      for (let i = 0; i < boxed.length; i++) {
        if (boxed[i][0](value)) return remember(NativeObject(boxed[i][1](value)));
      }
      if (ops.op_is_map(value)) {
        const result = remember(new NativeMap());
        const entries = new NativeMap();
        mapEach(value, (item, key) => mapSet(entries, key, item));
        mapEach(entries, (item, key) => mapSet(result, copy(key, memory), copy(item, memory)));
        return result;
      }
      if (ops.op_is_set(value)) {
        const result = remember(new NativeSet());
        const items = new NativeSet();
        setEach(value, (item) => setAdd(items, item));
        setEach(items, (item) => setAdd(result, copy(item, memory)));
        return result;
      }
      const prototype = getPrototypeOf(value);
      const array = isArray(value);
      if (prototype !== null && prototype !== (array ? ArrayPrototype : ObjectPrototype)) fail("Object");
      for (let i = 0; i < exotic.length; i++) if (exotic[i](value)) fail("Object");
      const result = remember(array ? [] : {});
      if (array) result.length = value.length;
      const keys = new NativeSet();
      const names = ownKeys(value);
      for (let i = 0; i < names.length; i++) {
        const key = names[i];
        if (typeof key === "string" && getOwnPropertyDescriptor(value, key).enumerable) setAdd(keys, key);
      }
      setEach(keys, (key) => {
        if (getOwnPropertyDescriptor(value, key) === undefined) return;
        defineProperty(result, key, {
          __proto__: null,
          value: copy(value[key], memory),
          writable: true,
          enumerable: true,
          configurable: true,
        });
      });
      return result;
    };
    return (value) => copy(value, new NativeMap());
  })();
  const encoder = new encoding.TextEncoder();
  const encodeInto = encoder.encodeInto.bind(encoder);
  class TextEncoder {
    get encoding() {
      return "utf-8";
    }
    encode(input = "") {
      input = String(input);
      // Allocate through the isolate allocator; core.encode adopts an untracked Vec.
      const bytes = new Uint8(input.length * 3);
      const { written } = encodeInto(input, bytes);
      return bytes.subarray(0, written);
    }
    encodeInto(source, destination) {
      return encodeInto(source, destination);
    }
  }
  Object.assign(globalThis, {
    TextEncoder,
    TextDecoder: encoding.TextDecoder,
    URL: url.URL,
    URLSearchParams: url.URLSearchParams,
    atob: base64.atob,
    btoa: base64.btoa,
    structuredClone(value, options) {
      if (options?.transfer?.length) throw new Error("Transfer lists unavailable in transactional execution");
      return clone(value);
    },
  });
  delete url.URL.createObjectURL;
  delete url.URL.revokeObjectURL;
  const levels = ["debug", "log", "info", "warn", "error"];
  globalThis.console = Object.freeze(
    Object.fromEntries(
      levels.map((level) => [
        level,
        (...values) => {
          const line = values
            .map((value) => {
              if (typeof value === "string") return value;
              try {
                return JSON.stringify(value);
              } catch {
                return String(value);
              }
            })
            .join(" ");
          log(level, line);
        },
      ]),
    ),
  );
  const integerArrays = new Set([
    Int8Array,
    Uint8Array,
    Uint8ClampedArray,
    Int16Array,
    Uint16Array,
    Int32Array,
    Uint32Array,
    BigInt64Array,
    BigUint64Array,
  ]);
  const getRandomValues = (array) => {
    if (!ArrayBuffer.isView(array) || !integerArrays.has(Object.getPrototypeOf(array).constructor))
      throw new TypeError("Expected an integer typed array");
    if (array.byteLength > 65536) throw new RangeError("Random byte limit exceeded");
    const bytes = new Uint8(array.buffer, array.byteOffset, array.byteLength);
    for (let i = 0; i < bytes.length; i++) bytes[i] = Math.floor(random() * 256);
    if (bytes.length === 0) random();
    return array;
  };
  globalThis.crypto = Object.freeze({
    getRandomValues,
    randomUUID() {
      const bytes = getRandomValues(new Uint8(16));
      bytes[6] = (bytes[6] & 15) | 64;
      bytes[8] = (bytes[8] & 63) | 128;
      const hex = Array.from(bytes, (n) => n.toString(16).padStart(2, "0")).join("");
      return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
    },
    subtle: Object.freeze({
      async digest(algorithm, input) {
        const name = String(typeof algorithm === "string" ? algorithm : algorithm.name).toUpperCase();
        const bytes = ArrayBuffer.isView(input)
          ? new Uint8(input.buffer, input.byteOffset, input.byteLength)
          : input instanceof ArrayBuffer
            ? new Uint8(input)
            : null;
        if (!bytes) throw new TypeError("Expected a buffer source");
        const result = new Uint8(
          name === "SHA-1" ? 20 : name === "SHA-256" ? 32 : name === "SHA-384" ? 48 : name === "SHA-512" ? 64 : 0,
        );
        digest(name, bytes, result);
        return result.buffer;
      },
    }),
  });
})();
