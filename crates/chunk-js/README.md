# Embedded transactional JavaScript

`Deployment::new(id, source, limits)` loads a bundled ES module into one persistent
`deno_core`/V8 runtime. `deployment.execute(invocation, host, cancellation)` selects
an exported function and calls it with `(ctx, arguments)`. Source and identity are
fixed at registration; calls supply export, arguments, caller, mode, snapshot timestamp
and seed. Imports
must be bundled before registration. Module initialization has no host capabilities.

The environment backend owns one `Deployment` per resident version. Each handle
starts a dedicated worker thread that creates, uses and drops its V8 runtime.
Calls require exclusive mutable access to the handle, allowing one outstanding
invocation with no growing internal queue. Construction and execution block; callers
should use their bounded blocking executor. Dropping the handle joins its worker.
The shared V8 platform is initialized before workers start. There is no global
deployment registry. Idle deployment workers sleep waiting for requests.

`ctx.db.get(table, id)` and `ctx.db.scan(table, start, end)` read a fresh memory-only
`ReadHost` snapshot on each call, recording dependencies and including speculative
writes. `ReadHost::get` and `scan` return raw snapshot data; the engine owns
invocation overlay merging and the `[id, value]` scan encoding. Mutations can `put(table, id, value)` and `delete(table, id)`. Queries cannot
write. Only successful execution returns writes; backend validation and atomic
storage commit are separate. No host method may publish external effects or block
on I/O. The watchdog cannot interrupt Rust host code.

Each invocation gets fresh caller data, capability budgets and speculative writes.
Host capabilities carry a generation checked by Rust; retaining an old `ctx.db`
cannot access a later call's snapshot or writes. Capabilities and the snapshot are
removed before execution returns. Module/global state survives successful calls.
It is disposable, never authoritative: handlers must not cache documents or caller
state, or use mutable counters to determine transactional results. Purity and complete
dependency tracking are application requirements, not enforced by context reuse.

The default budget is one second and 32 MiB of V8 heap, with a separate aggregate
live ArrayBuffer backing-store cap of the same size. Only `allocator.rs` allows
unsafe code for V8's allocator callbacks; the rest of the crate denies it.
A separate watchdog
interrupts synchronous loops and cancellation; the near-heap callback terminates
execution with 8 MiB of emergency headroom. Initialization and each invocation have
separate execution budgets. Any execution error drops the engine; the next call
reloads the same bundle under the initialization budget. Engines also recycle after
10,000 calls. Dropping a deployment releases its engine and retained source.

Input/result/document JSON is limited to 1 MiB, source to 4 MiB, capability calls to
4096 and writes to 256/8 MiB. JSON parsing enforces a nesting limit. The backend must
bound resident deployments, concurrent work, queued requests and snapshot memory;
the isolate heap budget is not a whole-process RSS limit. Release versions after
references drain. Retaining a bundle for a future job need not keep its engine alive;
admission, reference tracking and reload policy belong to the environment backend.

No filesystem, network, process, Node/Deno globals or runtime imports are exposed.
The backend supplies `Invocation::timestamp` in epoch milliseconds from its snapshot
and a `seed`, fixed for the operation and its retries. `Date.now()`, zero-argument
`new Date()` and `Date()` use that timestamp; explicit Date construction, parsing,
arithmetic and `instanceof` remain available. `Math.random()` uses a per-invocation
seeded SplitMix64 generator. Invocation time and randomness are unavailable during
module initialization. Locale APIs, weak references/finalizers and WebAssembly
remain unavailable.

ArrayBuffer, DataView and typed-array constructors remain available. Their live
backing stores share the isolate's bounded allocator, including retained globals.
Resizable buffers are rejected because V8 allocates their pages outside that
allocator; SharedArrayBuffer remains unavailable. Allocation budget failure rejects
the invocation and recycles the runtime, even if JS catches the allocation error.
Language objects, collections and Promises are supported. A returned Promise with
no possible completion fails; infinite microtask chains are interrupted. The event
loop drains before capabilities expire, and unhandled rejections fail the call.

The backend must propagate cancellation when request/session scope ends. Typed
contracts, the broader web API subset, conflict
retries and subscription scheduling remain outside this PR.

Focused verification: `cargo test -p chunk-js` and
`cargo clippy -p chunk-js --all-targets -- -D warnings`.

Termination records the first actual cancellation, deadline or heap signal. Teardown
time cannot turn a completed invocation into a deadline failure. Execution is bounded
by the watchdog without redundant Tokio timers. Static execution and capability
bounds live beside `Limits` in `model::bounds`. Stack traces identify the deployment.
