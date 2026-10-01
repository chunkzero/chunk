# chunk-js

The embedded JavaScript engine the [backend](../chunk-backend/README.md) runs a project's TypeScript functions on, after
[`chunk-build`](../chunk-build/README.md) has bundled them. It runs one bundled ES module per deployment on V8 (through
`deno_core`), with bounded time and memory, and gives code only the capabilities the backend hands it.

## API

`Engine::new()` owns one executor, one deadline watchdog and a registry of persistent V8 runtimes.
`register(DeploymentId, source, Limits)` loads a bundle; `execute(&id, Invocation, host, &Cancellation)` calls one of
its exports with `(ctx, args)`; `release(&id)` drops the runtime and its source. Call `Engine::init_platform` once on
the parent thread, then construct, use and drop each engine on a single thread: `Engine` is neither `Send` nor `Sync`,
and must not run inside a Tokio runtime. The backend owns admission, queues, storage and commits.

Queries and mutations read through a `ReadHost` (`get`, `scan`, and optionally `scan_index`), which returns snapshot
data and records dependencies; the host must bound its own reads and decoding, since the watchdog can't interrupt Rust
code. The engine merges the invocation's own writes into reads, and returns them only from successful calls; the backend
validates and commits them. Queries can't write.

`execute_action` runs an action with an `ActionHost` on a fresh isolate that is discarded afterwards, so action globals
never share state with transactional runtimes. Only actions get `runQuery`, `runMutation`, `sleep`, and the HTTP and
secret capabilities the host grants.

## What code can do

Functions must be deterministic. Module globals survive between calls but are disposable, so handlers must derive
results from their arguments, caller, controlled time and randomness, and tracked reads, never from cached documents or
mutable module state. Each invocation gets its time and seed from the backend: `Date.now()`, `new Date()` and
`Math.random()` use them, as do `crypto.randomUUID` and `getRandomValues`, which are therefore unsuitable for secrets.
Module initialization gets no time, randomness or capabilities.

Available: the language's objects, collections and promises; `URL`, `URLSearchParams`, `TextEncoder`/`TextDecoder`,
`atob`/`btoa`; `structuredClone` without transfer lists, of primitives other than symbols, plain objects (prototype
`Object.prototype` or `null`), arrays (prototype `Array.prototype` or `null`), `Map`, `Set`, `Date`, `RegExp`, boxed
numbers, strings, booleans and bigints, `ArrayBuffer`s, typed arrays and `DataView`s; anything else, including errors
and class instances, throws `DataCloneError`, unlike in browsers (an object whose internal state can't be read from
JavaScript, such as an iterator or a `URL`, clones as a plain object if its prototype was replaced to look like one);
`crypto.subtle.digest` with SHA-1/256/384/512 on up to 1 MiB; ordinary `ArrayBuffer`s and typed arrays; and `console`
methods, whose output (at most 32 messages and 16 KiB per call) the backend logs. Not available: filesystem, network,
process, timers, Node or Deno globals, runtime imports, `Intl` and locale methods, `performance`, weak references,
WebAssembly, `SharedArrayBuffer` and resizable `ArrayBuffer`s. A promise that can never settle fails the call, and so
does an unhandled rejection.

## Limits

| Limit                            | Default                                            |
| -------------------------------- | -------------------------------------------------- |
| Time per call                    | 1 second (at most 30)                              |
| V8 heap                          | 32 MiB (16 to 128); `ArrayBuffer` memory equals it |
| Source                           | 4 MiB                                              |
| Arguments, results and documents | 1 MiB of JSON each                                 |
| Capability calls per invocation  | 4096                                               |
| Distinct writes per invocation   | 256, 8 MiB in total                                |

A call that runs out of time, nears the heap limit (with 8 MiB of headroom to stop cleanly) or is cancelled terminates
its runtime, which reloads the bundle. Runtimes also recycle after 10,000 calls. These are logical limits, not an RSS
bound.

## Safety and tests

`unsafe` is denied in this crate, except in `src/isolate.rs`, which enters parked isolates, and `src/allocator.rs`,
which implements V8's `ArrayBuffer` allocator with the crate's memory accounting.

`cargo test -p chunk-js` runs the tests. `cargo bench -p chunk-js --bench engines -- <bundle directory>` compares this
engine with a plain V8 context on a compiled bundle, such as one [`chunk-bench`](../chunk-bench/README.md) writes.
