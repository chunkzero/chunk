# Embedded transactional JavaScript

`execute` evaluates a bundled ES module in a fresh `deno_core`/V8 isolate,
selects an exported function and calls it with `(ctx, arguments)`. The backend
supplies immutable deployment identity, validated arguments/caller context and
a memory-only `ReadHost`. Imports must be bundled before execution.

`ctx.db.get(table, id)` and `ctx.db.scan(table, start, end)` read the snapshot
through the host, which records dependencies and includes the speculative overlay.
Mutations can `put(table, id, value)` and `delete(table, id)`. Queries cannot write.
Only a successful invocation returns writes; backend validation and atomic storage
commit are separate. No host method may publish external effects.

The default budget is one second and 32 MiB of V8 heap. A separate watchdog
interrupts synchronous loops and cancellation; the near-heap callback terminates
execution with 8 MiB of emergency headroom. Input/result/document JSON is limited
to 1 MiB, source to 4 MiB, capability calls to 4096 and writes to 256/8 MiB.
JSON parsing enforces a nesting limit. The backend must bound concurrent execution
and snapshot memory; the isolate heap budget is not a whole-process RSS limit.

No filesystem, network, process, Node/Deno globals or runtime imports are exposed.
Wall-clock time, random numbers, locale APIs, weak references/finalizers and
WebAssembly are unavailable. ArrayBuffer and typed-array constructors are also
unavailable in this initial profile so backing stores cannot bypass the heap
budget. Language objects, collections and Promises are supported. A Promise with
no possible completion fails; an infinite microtask chain is interrupted.

Run execution on a bounded blocking executor. Cancellation must be propagated by
the backend when request/session scope ends. Globals are discarded after every
invocation, including between calls to the same deployment. Transactions must not
use globals as authoritative state. Exact typed contracts and conflict retries
belong to the environment backend.

Focused verification: `cargo test -p chunk-js` and
`cargo clippy -p chunk-js --all-targets -- -D warnings`.
