# Embedded transactional JavaScript

`Engine::new()` owns one caller-thread executor, one deadline watchdog, and a
registry of persistent V8 runtimes. `register(DeploymentId, source, limits)` loads
a bundled ES module; `execute(&id, invocation, host, cancellation)` calls an export
with `(ctx, arguments)`. `release(&id)` drops the runtime and retained source.
Duplicate identities fail. Bundle imports before registration; module initialization
has no invocation capabilities. Stack traces identify the deployment.

Construct, use and drop the engine on exactly one environment thread. `Engine` is
neither `Send` nor `Sync`; initialize its V8 platform on the common parent before
spawning environment threads. The backend serializes execution and owns admission,
version retention, queues and storage commits. Do not call this synchronous engine
inside another Tokio runtime. The single-deployment owner is only a test helper.

The private `isolate.rs` wrapper enters parked isolates for access and destruction,
restoring the prior isolate on return or unwind. Only that wrapper and `allocator.rs`
allow unsafe code; the rest of the crate denies it. The allocator implements V8's
backing-store callbacks, with aggregate accounting across concurrent GC frees.

`ReadHost::get`, `scan`, and optional `scan_index` return snapshot data and record dependencies.
Indexed reads also return declared index fields; the engine merges invocation
writes before applying the index order and result limit. Hosts must enforce a
cumulative decoding budget before allocating documents.
The engine merges invocation-local puts/deletes and encodes scans as `[id, value]`
pairs. Queries cannot write. Only successful calls return speculative writes; the
backend validates and commits them. Hosts must publish no external effects and
bound their own reads: the watchdog cannot interrupt Rust host code. The backend
currently performs synchronous SQLite snapshot reads and decoding.

Each invocation gets fresh caller data, capabilities and write budgets. Generation
checks reject retained database capabilities. Capabilities and profile context expire
after the event-loop drain. Module globals survive success and ordinary application
errors, but remain disposable: handlers must derive results from arguments, caller,
controlled time/randomness and tracked reads, never cached documents or mutable
module counters. Purity and complete dependency tracking are application requirements.

The default budget is one second and 32 MiB of V8 heap. A separate aggregate live
ArrayBuffer backing-store budget equals the heap budget. Neither is an RSS limit.
The watchdog sleeps indefinitely while idle and polls every two milliseconds while
armed. Cancellation, deadlines, near-heap exhaustion and denied buffer allocations
record the first termination reason; teardown time cannot change a completed call
into a deadline failure. Near-heap termination gets 8 MiB of emergency headroom.
Terminated runtimes reload the bundle; ordinary errors drain and retain it. Runtimes
also recycle after 10,000 calls, independently of other deployments.

Input/result/document JSON is limited to 1 MiB, source to 4 MiB, capability calls to
4096 and distinct writes to 256/8 MiB. Static bounds live beside `Limits` in
`model::bounds`. The result boundary validates storage-compatible JSON depth and
Unicode, then returns JSON text for forwarding. Top-level `undefined` becomes `null`;
nested non-JSON values are rejected. One bootstrap invocation constructs context,
awaits the handler and serializes the result, followed by event-loop drain.

The backend supplies snapshot-acquisition `timestamp` milliseconds and a `seed` on
`Invocation`. `Date.now()`, `Date()` and zero-argument `new Date()` use that time;
explicit Date construction, parsing, arithmetic and `instanceof` remain available.
`Math.random()` uses deterministic SplitMix64. Invocation time/randomness fail at
module initialization; any internal retry must reuse both values. Ordinary
ArrayBuffer, DataView and typed arrays are supported. Resizable ArrayBuffers remain
unavailable because V8 bypasses the custom allocator for their backing stores.

No filesystem, network, process, Node/Deno globals or runtime imports are exposed.
Locale methods, Intl, performance, weak references/finalizers, WebAssembly and
SharedArrayBuffer remain unavailable. Language objects, collections and Promises
are supported. A promise with no possible completion fails; infinite microtask chains
are interrupted, and unhandled rejections fail the invocation.

Focused checks: `cargo test -p chunk-js` and
`cargo clippy -p chunk-js --all-targets -- -D warnings`.

The fixed web subset uses the pinned Deno web implementations for URL/search
parameters, text decoding and base64. Text encoding allocates through the bounded
isolate allocator. `structuredClone` supports ordinary structured data, including
cycles, maps, sets, dates and typed arrays; transfer lists are rejected. Streams,
networking, timers, object URLs and extension imports remain unavailable.

`crypto.randomUUID` and integer-array `getRandomValues` use the invocation seed;
these deterministic values are not suitable for secrets. `subtle.digest` supports
SHA-1/256/384/512 with at most 1 MiB per input. Console debug/log/info/warn/error
collect at most 32 messages and 16 KiB per successful invocation, returned as
`Execution.logs`. Exceeding the limit fails the invocation. Initialization cannot
log or obtain randomness. The backend emits successful evaluation logs through
its tracing subscriber; logs are diagnostics, not transactional effects.

The expanded profile requires at least 16 MiB of managed heap (32 MiB by default).
Managed heap and aggregate buffer limits remain separate. Encoder and clone
allocation exhaustion is tested together with runtime recycling.
