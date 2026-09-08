# Embedded transactional JavaScript

`Engine::new()` owns one current-thread executor, one deadline watchdog, and a
registry of persistent `deno_core`/V8 runtimes. `register(DeploymentId, source,
limits)` loads a bundled ES module; `execute(&id, invocation, host, cancellation)`
calls an export with `(ctx, arguments)`. `release(&id)` immediately drops its
runtime and retained source, independent of registration order. Duplicate
registration fails. Imports must be bundled; initialization has no capabilities.

The backend constructs, uses and drops `Engine` on exactly one environment engine
thread and serializes calls there. `Engine` is neither `Send` nor `Sync`. Call
`Engine::init_platform()` on the common parent before spawning environment threads.
Network tasks use the backend's bounded request channel; JavaScript and sync
orchestration share the engine thread without a channel hop between evaluations.
Do not execute inside another Tokio runtime; these methods synchronously drive
the engine's own executor. `Deployment` is a convenience wrapper over an `Engine`
with one registered version, with the same caller-thread contract.

The private `isolate.rs` wrapper exits idle isolates and enters them only for use
or destruction. Its scoped guard restores the previous isolate on return and
unwind. Only that module allows unsafe entry/exit calls; the rest of this crate
denies unsafe code and other workspace crates retain their `forbid` lint.

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
per engine polls deadlines and cancellation every two milliseconds and shuts down
with the engine. It interrupts synchronous loops and cancellation; the near-heap
callback terminates execution with 8 MiB of emergency headroom. Initialization and each invocation have
separate execution budgets. Any execution error drops that deployment runtime; the next call
reloads the same bundle under the initialization budget. Each deployment runtime also recycles after
10,000 calls, without disturbing other versions. Dropping the engine releases all
runtimes and joins its watchdog.

Input/result/document JSON is limited to 1 MiB, source to 4 MiB, capability calls to
4096 and writes to 256/8 MiB. Host document JSON parsing enforces a nesting limit.
Results remain strict JSON text in `Execution::value`; consumers can forward them without parsing. Caller and
arguments are encoded once and parsed in JS. A single bootstrap entry constructs
the context, awaits the handler and serializes its result, followed by an event-loop
drain. The backend must bound resident deployments, concurrent work, queued requests and snapshot memory;
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
