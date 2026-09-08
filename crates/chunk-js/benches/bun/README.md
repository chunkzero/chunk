# Bun engine experiment

Temporary comparison of running the environment backend's JavaScript directly in
Bun (JavaScriptCore) against the V8 harness in `../engine`. Both harnesses share
the same fixture text, bootstrap semantics, snapshot host with dependency and write
recording, warm-up/sample counts, result checks and a small single-writer sync
engine. `parity.py` runs every cell on both sides with reduced counts and asserts
identical bundle bytes, evaluation counts, publication counts and final state.

## Run

```sh
python3 crates/chunk-js/benches/bun/smoke.py            # functional pass, small counts
python3 crates/chunk-js/benches/bun/parity.py           # both harnesses agree
python3 crates/chunk-js/benches/bun/run.py unrestricted # same systemd cgroup as ../engine
python3 crates/chunk-js/benches/bun/run.py quota --engines inline persistent fresh-vm --workloads query --sizes 0 128 --bursts 10 --init declarations
python3 crates/chunk-js/benches/report.py               # tables from both result sets
taskset -c 4-7 bun crates/chunk-js/benches/bun/unload.ts         # module/context retention
taskset -c 4-7 bun crates/chunk-js/benches/bun/unload-workers.ts # worker teardown
```

`main.ts ENGINE WORKLOAD BUNDLE_KIB [BURST_PER_80MS]` mirrors the Rust binary.
`BENCH_WARMUP`/`BENCH_CALLS` override sample counts on both harnesses.

## Engines

| Bun engine | Shape | V8 counterpart |
| --- | --- | --- |
| `inline` | Main thread, ES module imported once, deterministic time/random installed on the shared global. No thread hop, no capability removal. | none (floor) |
| `persistent` | Dedicated `Worker` per deployment, ES module imported once via blob URL, hardened worker global, `postMessage` round trip per call, one-second deadline that terminates the worker. | `persistent` |
| `persistent-vm` | Worker, one `node:vm` context created once and reused. | `persistent` |
| `fresh-vm` | Worker, new `vm` context + bootstrap + bundle script per call (script compiled once, `cachedData` attached). | `fresh` |
| `fresh-esm` | Worker, new `vm` context + `vm.SourceTextModule` compile/link/evaluate per call. | `fresh` (module semantics) |
| `fresh-realm` | Worker, new `ShadowRealm` per call; arguments, reads, writes and results cross as JSON text. | `fresh` |

Primitives (`empty` workload, 128 KiB bundle): `context` (vm context creation),
`realm` (ShadowRealm creation + trivial evaluate), `worker` (spawn, ready message,
terminate, close), `cold` (fresh context + full ES-module compile/link/evaluate),
`cached` (bundle script run in a reused context with or without `cachedData`),
`terminate` (worker in a synchronous loop: `terminate()` to `close`), `vmtimeout`
(vm watchdog overshoot beyond its minimum 1 ms budget for a synchronous loop).

The `sync` workload is one operation: a `bump` mutation (one point read, one write),
read-set validation against row revisions, atomic apply, then re-evaluation of
the subscribed `query` handlers whose recorded points/ranges intersect the write.
Five subscriptions exist; every operation re-evaluates exactly two. Per-operation
wall/CPU therefore covers three evaluations plus engine-side bookkeeping.

## Equivalences and differences from the V8 harness

- Host reads return live objects into the context (vm engines) or the worker
  global; the V8 harness builds them through `serde_v8`. ShadowRealm uses JSON text.
- The Rust worker arms a watchdog channel per call; the Bun main thread arms a
  timer per call that terminates the worker. Neither fires in these cells.
- V8 cells enforce a 32 MiB heap limit per isolate. Bun has no per-worker or
  per-context heap limit; the cgroup's 512 MiB `MemoryMax` is the only bound.
- V8 `fresh` consumes a code cache; Bun `fresh-vm` attaches `cachedData`, which
  Bun 1.4 accepts without rejecting. The `cached` primitive with `BENCH_CACHE=none`
  shows whether it changes anything.
- Microtasks drain explicitly via `bun:jsc` `drainMicrotasks()` after each handler,
  and a still-pending promise is a failure, like the V8 checkpoint.
- Bun's global hardening is best effort: `Bun` is non-configurable on worker and
  ShadowRealm globals, `import()` syntax stays available, and vm contexts are not
  frozen because freezing a vm global breaks builtin lookup in Bun 1.4.
