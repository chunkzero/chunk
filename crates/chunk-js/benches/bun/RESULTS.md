# Bun versus embedded V8 — 2026-09-07

Question: could the environment backend run directly as a Bun server, with no
Rust, and what would that cost? This records measurements from `bun/` against the
V8 harness in `engine/` after both were brought to functional parity
(`bun/parity.py` passes on every cell). Bun 1.4.0 (JavaScriptCore), V8 150.4.0 via
`deno_core` 0.411.0, Rust 1.98.1. Local machine: Ryzen 7 7840HS, cells pinned to
CPUs 0–1 inside a 512 MiB systemd scope, run sequentially. Every cell is 1,000
warm-up and 10,000 measured calls with result checks. No Fly machine was used.

The harness fixture changed for this comparison: the tiny bundle is now 681 bytes
(a `bump` mutation and query arguments were added) instead of the 452 bytes in
`engine/RESULTS.md`, and a `sync` workload exists. Rust cells were re-run; the
earlier V8 numbers remain consistent with the new ones.

## Summary

- **Steady-state persistent workers are equal or better in Bun.** The Bun worker
  (one deployment per `Worker`, module loaded once, `postMessage` per call) costs
  about 0.03 ms CPU per query versus 0.06 ms for the V8 worker, and 0.11 ms versus
  0.11 ms per sync operation (three evaluations). Calling handlers inline on the
  main thread with no isolation costs 0.01 ms.
- **Fresh-context-per-call is worse in Bun.** JSC has no usable code cache
  through `vm`: a fresh context that evaluates the eagerly initialized 143 KB
  bundle costs 2.1–2.5 ms in Bun versus 0.8–0.9 ms in V8. With declarations only,
  the gap narrows (0.29 versus 0.52 ms median under quota) because JSC parses
  lazily. `ShadowRealm` and `vm.SourceTextModule` are slower still.
- **Bun's memory floor is roughly double**: peak RSS 50–100 MiB per cell versus
  26–43 MiB. Terminated workers return their memory; distinct ES module
  instances never do; dropped `vm` contexts are collected only when GC decides.
- **Under the 10 ms / 80 ms quota both persistent designs keep up at 125 calls/s**
  with sub-2 ms response p99. The Bun worker showed two 70 ms stalls per cell
  where V8's worst was about 6 ms; V8 fresh contexts show the same stalls.
- **No per-worker heap limit, no capability-free global, no unload API.** These
  are the architectural caveats; the numbers above are not the blocker.


## Measurements

### Unrestricted local comparison

Closed loop, no CPU quota. `sync` rows are per operation (three evaluations).

| Bundle bytes | Workload | Engine | Wall median ms | Wall p99 ms | Mean process CPU ms/call | Peak RSS MiB |
| ---: | --- | --- | ---: | ---: | ---: | ---: |
| 681 | empty | rust-deno | 4.287 | 4.827 | 4.691 | 29.2 |
| 681 | empty | rust-fresh | 0.236 | 0.668 | 0.265 | 31.6 |
| 681 | empty | rust-persistent | 0.029 | 0.042 | 0.034 | 26.5 |
| 681 | empty | bun-inline | 0.004 | 0.008 | 0.006 | 50.0 |
| 681 | empty | bun-persistent | 0.017 | 0.033 | 0.027 | 64.9 |
| 681 | empty | bun-persistent-vm | 0.017 | 0.031 | 0.027 | 62.4 |
| 681 | empty | bun-fresh-vm | 0.108 | 0.407 | 0.127 | 83.8 |
| 681 | empty | bun-fresh-esm | 0.138 | 0.498 | 0.163 | 96.6 |
| 681 | empty | bun-fresh-realm | 0.348 | 0.739 | 0.387 | 75.9 |
| 681 | reads | rust-deno | 4.272 | 4.738 | 4.688 | 29.0 |
| 681 | reads | rust-fresh | 0.261 | 0.751 | 0.293 | 33.1 |
| 681 | reads | rust-persistent | 0.041 | 0.058 | 0.048 | 38.4 |
| 681 | reads | bun-inline | 0.004 | 0.008 | 0.007 | 51.1 |
| 681 | reads | bun-persistent | 0.018 | 0.035 | 0.029 | 63.2 |
| 681 | reads | bun-persistent-vm | 0.019 | 0.034 | 0.029 | 61.4 |
| 681 | reads | bun-fresh-vm | 0.120 | 0.399 | 0.142 | 70.1 |
| 681 | reads | bun-fresh-esm | 0.156 | 0.559 | 0.185 | 94.8 |
| 681 | reads | bun-fresh-realm | 0.380 | 0.800 | 0.421 | 77.7 |
| 681 | query | rust-deno | 4.361 | 4.829 | 4.758 | 29.5 |
| 681 | query | rust-fresh | 0.285 | 1.031 | 0.325 | 33.9 |
| 681 | query | rust-persistent | 0.052 | 0.071 | 0.060 | 38.9 |
| 681 | query | bun-inline | 0.008 | 0.015 | 0.013 | 53.3 |
| 681 | query | bun-persistent | 0.023 | 0.051 | 0.036 | 67.8 |
| 681 | query | bun-persistent-vm | 0.023 | 0.046 | 0.035 | 66.0 |
| 681 | query | bun-fresh-vm | 0.145 | 0.534 | 0.172 | 84.4 |
| 681 | query | bun-fresh-esm | 0.176 | 0.601 | 0.207 | 94.5 |
| 681 | query | bun-fresh-realm | 0.408 | 0.962 | 0.462 | 77.5 |
| 681 | sync | rust-deno | 13.002 | 14.060 | 14.257 | 31.9 |
| 681 | sync | rust-fresh | 0.816 | 1.458 | 0.885 | 35.2 |
| 681 | sync | rust-persistent | 0.094 | 0.119 | 0.106 | 41.9 |
| 681 | sync | bun-inline | 0.011 | 0.020 | 0.018 | 56.4 |
| 681 | sync | bun-persistent | 0.079 | 0.231 | 0.107 | 68.2 |
| 681 | sync | bun-persistent-vm | 0.079 | 0.239 | 0.108 | 65.3 |
| 681 | sync | bun-fresh-vm | 0.470 | 0.964 | 0.592 | 79.0 |
| 681 | sync | bun-fresh-esm | 0.567 | 1.156 | 0.694 | 99.9 |
| 681 | sync | bun-fresh-realm | 1.253 | 2.169 | 1.453 | 88.0 |
| 143077 | empty | rust-fresh | 0.769 | 1.677 | 0.880 | 39.3 |
| 143077 | empty | rust-persistent | 0.028 | 0.041 | 0.034 | 28.7 |
| 143077 | empty | bun-inline | 0.004 | 0.008 | 0.006 | 53.2 |
| 143077 | empty | bun-persistent | 0.017 | 0.031 | 0.027 | 66.0 |
| 143077 | empty | bun-persistent-vm | 0.017 | 0.030 | 0.027 | 63.1 |
| 143077 | empty | bun-fresh-vm | 2.029 | 2.701 | 2.302 | 71.8 |
| 143077 | empty | bun-fresh-esm | 3.687 | 5.494 | 4.134 | 82.4 |
| 143077 | empty | bun-fresh-realm | 2.351 | 3.681 | 2.990 | 75.9 |
| 143077 | reads | rust-fresh | 0.802 | 1.807 | 0.931 | 39.6 |
| 143077 | reads | rust-persistent | 0.041 | 0.057 | 0.048 | 39.6 |
| 143077 | reads | bun-inline | 0.004 | 0.009 | 0.007 | 54.6 |
| 143077 | reads | bun-persistent | 0.019 | 0.037 | 0.030 | 65.8 |
| 143077 | reads | bun-persistent-vm | 0.019 | 0.037 | 0.030 | 62.4 |
| 143077 | reads | bun-fresh-vm | 1.975 | 3.059 | 2.424 | 71.9 |
| 143077 | reads | bun-fresh-esm | 3.655 | 5.061 | 4.121 | 83.5 |
| 143077 | reads | bun-fresh-realm | 2.409 | 4.134 | 3.138 | 78.2 |
| 143077 | query | rust-deno | 11.579 | 12.454 | 11.981 | 31.0 |
| 143077 | query | rust-fresh | 0.821 | 1.740 | 0.940 | 39.2 |
| 143077 | query | rust-persistent | 0.052 | 0.069 | 0.060 | 40.8 |
| 143077 | query | bun-inline | 0.008 | 0.014 | 0.013 | 55.6 |
| 143077 | query | bun-persistent | 0.023 | 0.043 | 0.035 | 69.5 |
| 143077 | query | bun-persistent-vm | 0.023 | 0.041 | 0.035 | 65.5 |
| 143077 | query | bun-fresh-vm | 2.106 | 3.120 | 2.546 | 72.9 |
| 143077 | query | bun-fresh-esm | 3.760 | 5.527 | 4.203 | 80.0 |
| 143077 | query | bun-fresh-realm | 2.594 | 3.981 | 3.171 | 81.1 |
| 143077 | sync | rust-deno | 33.220 | 35.822 | 34.329 | 32.8 |
| 143077 | sync | rust-fresh | 2.472 | 3.536 | 2.729 | 40.9 |
| 143077 | sync | rust-persistent | 0.097 | 0.121 | 0.109 | 43.4 |
| 143077 | sync | bun-inline | 0.012 | 0.018 | 0.018 | 59.1 |
| 143077 | sync | bun-persistent | 0.078 | 0.251 | 0.108 | 71.0 |
| 143077 | sync | bun-persistent-vm | 0.079 | 0.241 | 0.108 | 67.4 |
| 143077 | sync | bun-fresh-vm | 6.351 | 8.100 | 7.735 | 75.7 |
| 143077 | sync | bun-fresh-esm | 10.549 | 13.675 | 12.625 | 87.5 |
| 143077 | sync | bun-fresh-realm | 7.873 | 9.791 | 9.487 | 88.6 |

### Component measurements (128 KiB bundle)

Median/p99 are wall time except `*-terminate` and `bun-vmtimeout` (termination latency) and `bun-cold` (module phase).

| Operation | Median ms | p99 ms | Mean process CPU ms | Peak RSS MiB |
| --- | ---: | ---: | ---: | ---: |
| rust-isolate | 0.461 | 0.561 | 0.451 | 25.6 |
| rust-context | 0.167 | 0.987 | 0.193 | 36.2 |
| rust-cold | 7.675 | 8.126 | 7.825 | 29.2 |
| rust-cached | 0.522 | 0.962 | 0.586 | 41.3 |
| rust-terminate | 0.008 | 0.011 | 0.818 | 22.1 |
| bun-cached (no cache) | 1.753 | 2.537 | 1.770 | 66.1 |
| bun-fresh-vm (no cache) | 2.050 | 3.021 | 2.301 | 72.0 |
| bun-context | 0.065 | 5.902 | 0.353 | 539.2 |
| bun-realm | 0.052 | 0.130 | 0.058 | 84.6 |
| bun-worker | 0.726 | 1.693 | 0.802 | 49.9 |
| bun-cold | 10.158 | 1606.209 | 27.771 | 540.3 |
| bun-cached | 1.572 | 2.399 | 1.801 | 64.8 |
| bun-terminate | 0.390 | 1.278 | 0.828 | 50.5 |
| bun-vmtimeout | 0.097 | 3.793 | 2.605 | 540.5 |

### Local 10 ms / 80 ms CPU quota, burst arrivals

`CPUQuota=12.5%` with an 80 ms period, the aggregate of a Fly `shared-cpu-2x` without burst credits. Large bundle is declarations plus registry without eager initialization, matching the Fly rows in `engine/RESULTS.md`. Response time starts at the scheduled arrival. `nr_periods` counts only periods in which the scope ran.

| Bundle bytes | Workload | Engine | Arrivals per 80 ms | Offered/s | Completed/s | Wall median ms | Wall p99 ms | Mean process CPU ms/call | Response p99 ms | Max response ms | Throttled periods |
| ---: | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 681 | query | rust-fresh | 10 | 125.0 | 125.1 | 0.353 | 1.542 | 0.431 | 6.037 | 8.863 | 0/999 |
| 681 | query | rust-persistent | 10 | 125.0 | 125.1 | 0.100 | 0.173 | 0.108 | 1.231 | 5.868 | 0/999 |
| 681 | query | bun-inline | 10 | 125.0 | 125.1 | 0.015 | 0.060 | 0.036 | 0.343 | 1.022 | 0/525 |
| 681 | query | bun-persistent | 10 | 125.0 | 125.1 | 0.093 | 0.235 | 0.111 | 1.380 | 72.464 | 2/997 |
| 681 | query | bun-fresh-vm | 10 | 125.0 | 125.1 | 0.229 | 0.747 | 0.310 | 3.187 | 7.013 | 1/999 |
| 681 | sync | rust-fresh | 5 | 62.5 | 62.5 | 0.964 | 2.074 | 1.061 | 6.366 | 75.026 | 3/1999 |
| 681 | sync | rust-persistent | 5 | 62.5 | 62.5 | 0.194 | 0.328 | 0.209 | 1.237 | 5.271 | 0/1999 |
| 681 | sync | bun-inline | 5 | 62.5 | 62.5 | 0.023 | 0.080 | 0.056 | 0.391 | 15.989 | 3/845 |
| 681 | sync | bun-persistent | 5 | 62.5 | 62.5 | 0.160 | 0.554 | 0.280 | 1.548 | 3.901 | 1/1999 |
| 681 | sync | bun-fresh-vm | 5 | 62.5 | 62.5 | 0.679 | 1.543 | 0.943 | 4.547 | 73.854 | 3/1999 |
| 142935 | query | rust-fresh | 10 | 125.0 | 125.1 | 0.515 | 2.925 | 0.649 | 8.745 | 75.994 | 2/999 |
| 142935 | query | rust-persistent | 10 | 125.0 | 125.1 | 0.100 | 0.213 | 0.113 | 1.405 | 5.052 | 0/999 |
| 142935 | query | bun-inline | 10 | 125.0 | 125.1 | 0.015 | 0.062 | 0.036 | 0.349 | 1.404 | 0/516 |
| 142935 | query | bun-persistent | 10 | 125.0 | 125.1 | 0.045 | 0.223 | 0.083 | 1.442 | 71.092 | 2/990 |
| 142935 | query | bun-fresh-vm | 10 | 125.0 | 125.1 | 0.293 | 0.857 | 0.390 | 3.764 | 5.671 | 1/999 |
| 142935 | sync | rust-fresh | 5 | 62.5 | 62.5 | 1.563 | 63.319 | 1.735 | 73.409 | 80.511 | 150/1999 |
| 142935 | sync | rust-persistent | 5 | 62.5 | 62.5 | 0.199 | 0.496 | 0.211 | 1.318 | 5.606 | 0/1999 |
| 142935 | sync | bun-inline | 5 | 62.5 | 62.5 | 0.023 | 0.079 | 0.057 | 0.412 | 1.127 | 1/846 |
| 142935 | sync | bun-persistent | 5 | 62.5 | 62.5 | 0.210 | 0.569 | 0.308 | 1.620 | 3.816 | 1/1999 |
| 142935 | sync | bun-fresh-vm | 5 | 62.5 | 62.5 | 0.816 | 1.857 | 1.134 | 5.188 | 77.131 | 7/1999 |

`bun-context`, `bun-cold` and `bun-vmtimeout` create a `vm` context per sample
with nothing holding it; RSS climbed to the 512 MiB scope limit and the cold-load
p99 reached 1.6 s from GC thrash near the limit. JSC collects contexts lazily and
Bun exposes no heap limit to force it earlier. `bun-cached` with and without
`cachedData` differ by about 10%, so Bun 1.4 accepts the cache but does not gain
V8's 15× cached-load advantage (0.52 ms versus 7.7 ms cold).

`bun-terminate` measures a worker in a synchronous loop from `terminate()` to
the `close` event: 0.39 ms median, 1.28 ms p99, versus 0.008 ms for V8's
`TerminateExecution` acknowledgement. `bun-vmtimeout` is the vm watchdog's
overshoot beyond its 1 ms minimum for a synchronous loop: 0.10 ms median,
3.8 ms p99. Neither can stop a loop inside a microtask that never yields
without terminating the whole worker.

## Dynamic loading and unloading (Bun 1.4.0)

Measured with `bun/unload.ts` and `bun/unload-workers.ts` on the 143 KB
declarations bundle:

| Mechanism | Load cost | Unload | Memory after release |
| --- | ---: | --- | --- |
| `import()` of a new blob/file URL per deployment | ~0.8 MiB per instance | none; registry keyed by URL, `revokeObjectURL` does not evict | 300 instances: +236 MiB retained after full GC |
| `vm.createContext` + script evaluate | 0.07 ms context, ~2 ms bundle | drop references | 300 contexts: +102 MiB, returned after `Bun.gc(true)` |
| `Worker` per deployment | 4.5 ms spawn + bundle load, 2.6 MiB each | `terminate()`; `close` in ~2 ms with a message handler installed | 40 workers: 127 → 37 MiB |

Consequences:

- The only reliable unload is worker teardown, which matches D36's one runtime
  per deployment. Rolling a new deployment inside an existing worker leaks.
- A worker that has no message handler and no pending work exits by itself,
  so a deployment worker must keep its handler installed for its whole life.
- `resourceLimits` is accepted and ignored: a worker allocated 1.6 GiB without
  error. A worker can poll its own `bun:jsc` `heapSize()` and abort, which is
  cooperative and coarse (stopped at 81 MiB against a 64 MiB threshold when
  checked every 100 iterations); the main thread cannot observe another
  worker's heap. Memory admission has to be process-level (cgroup) plus
  self-reporting, and an overrun kills the whole backend, not one deployment.

## Caveats of a Bun-only backend

1. **Isolation is by thread, not by engine boundary.** Each `Worker` is its own
   JSC VM, which gives real heap separation and termination. Inside a worker,
   `vm` contexts give a fresh global with no ambient `fetch`/`process`/`Bun`, but
   host objects passed in expose the outer realm through their prototype chain
   (`this.constructor.constructor` reaches the host `Function`), so they are a
   determinism tool, not a security boundary. `ShadowRealm` has separate
   intrinsics but Bun installs its full global there, including a
   non-configurable `Bun` namespace with filesystem, network and subprocess
   access. `Bun.plugin` cannot intercept builtin specifiers, so `import("node:fs")`
   inside customer code always works. The knowledgebase's "no ambient
   filesystem, subprocesses, or runtime package installation" becomes a
   convention enforced by bundling and linting, not by the runtime.
2. **No heap limit, no cancellation primitive below the worker.** V8 gives a
   per-isolate heap limit and `TerminateExecution` in 8 µs. Bun gives
   `terminate()` for the whole worker (0.4 ms median, up to a few ms) and a
   vm timeout for synchronous scripts only. Recycling after a runaway call is
   worker replacement, which costs ~5 ms plus warm-up loss.
3. **No code cache or heap snapshot.** JSC compiles lazily and fast, so a
   warm persistent worker is cheap, but every new worker or context re-parses
   the bundle. D36's persistent design avoids this on the hot path; recycling
   and multi-deployment churn pay it.
4. **Memory.** Bun's process floor plus one VM per worker roughly doubles RSS
   versus the Rust binary. On a 512 MiB non-production machine that leaves less
   room for retained deployments and for SQLite page cache.
5. **Behaviour differences found while building the harness.** Freezing a vm
   context global makes builtins resolve to `undefined` afterwards; `Object.freeze(globalThis)`
   on the worker global works. Node-style `resourceLimits` is silently ignored.
   `vm.SourceTextModule` exists and works but costs 1.5–1.8× a script evaluate.
   These are Bun 1.4 observations and may change between minor versions, which
   is itself a caveat: the runtime's semantics are less stable than V8 embedding.
6. **What Bun makes easier.** `bun:sqlite`, `Bun.sql` (Postgres/MySQL) and the
   Turso SDK cover every planned storage adapter without an FFI layer; the
   toolchain, bundler and function SDK are already JavaScript; one language for
   backend, SDK and dashboard; and a warm worker call is faster than the V8
   worker call. The proxy (raw Minecraft TCP, protocol crates from #19) and the
   gRPC transport to JVMs would remain Rust or need a Bun HTTP/2 server path
   that was not evaluated here.
7. **Not measured.** Storage round trips, subscription fan-out delivery, the
   web API subset, real bundles, Fly hardware, and Bun's HTTP/2 or gRPC server
   behaviour. The sync engine here is in-memory and single-writer.

## Assessment

If the engine stays on D36's persistent-worker model, Bun's per-call cost is not
a problem: it is at least as fast as the V8 worker and simpler to build. The
decision turns on whether the platform needs engine-level guarantees that Bun
cannot give: per-deployment heap limits, sub-millisecond cancellation of any
call, a global with no ambient capabilities, and memory that unloads with the
deployment. For a single-tenant environment backend where customer code is the
environment owner's own, those are robustness features rather than security
features, and process-level limits plus worker recycling may be acceptable. For
anything multi-tenant, or if fresh-context isolation per call is ever wanted
again, V8 is the only one of the two that can do it at the measured cost.
