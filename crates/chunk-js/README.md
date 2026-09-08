# Embedded transactional JavaScript

`Deployment::new(id, source, limits)` loads a bundled ES module into one persistent
`deno_core`/V8 runtime. `deployment.execute(invocation, host, cancellation)` selects
an exported function and calls it with `(ctx, arguments)`. Source and identity are
fixed at registration; calls supply only export, arguments, caller and mode. Imports
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
writes. Mutations can `put(table, id, value)` and `delete(table, id)`. Queries cannot
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

The default budget is one second and 32 MiB of V8 heap. One persistent watchdog
per worker polls deadlines and cancellation every two milliseconds and shuts down
with the worker. It interrupts synchronous loops and cancellation; the near-heap
callback terminates
execution with 8 MiB of emergency headroom. Initialization and each invocation have
separate execution budgets. Any execution error drops the engine; the next call
reloads the same bundle under the initialization budget. Engines also recycle after
10,000 calls. Dropping a deployment releases its engine and retained source.

Input/result/document JSON is limited to 1 MiB, source to 4 MiB, capability calls to
4096 and writes to 256/8 MiB. Host document JSON parsing enforces a nesting limit.
Results remain strict JSON
text in `Execution::value`; consumers can forward them without parsing. Caller and
arguments are encoded once and parsed in JS. A single bootstrap entry constructs
the context, awaits the handler and serializes its result, followed by an event-loop
drain. The backend must
bound resident deployments, concurrent work, queued requests and snapshot memory;
the isolate heap budget is not a whole-process RSS limit. Release versions after
references drain. Retaining a bundle for a future job need not keep its engine alive;
admission, reference tracking and reload policy belong to the environment backend.

No filesystem, network, process, Node/Deno globals or runtime imports are exposed.
Wall-clock time, random numbers, locale APIs, weak references/finalizers and
WebAssembly are unavailable. ArrayBuffer and typed-array constructors are also
unavailable in this initial profile so backing stores cannot bypass the heap budget.
Language objects, collections and Promises are supported. A returned Promise with
no possible completion fails; infinite microtask chains are interrupted. The event
loop drains before capabilities expire, and unhandled rejections fail the call.

The backend must propagate cancellation when request/session scope ends. Typed
contracts, deterministic time/random APIs, the broader web API subset, conflict
retries and subscription scheduling remain outside this PR.

Focused verification: `cargo test -p chunk-js` and
`cargo clippy -p chunk-js --all-targets -- -D warnings`.
