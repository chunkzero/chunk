# Engine performance experiment

This standalone benchmark workspace now extracts the persistent PR #21 implementation
at `97f0ee1`. Earlier recorded `deno` results use `a124f1a`, which recreated runtimes
per call. It also includes a small direct-V8 prototype. It does not implement D36 or change
the production `chunk-js` API. Both engines use V8 150.4.0 through the pinned
`deno_core` 0.411.0 dependency; the direct path calls V8 directly and uses
`serde_v8` for host values. The extracted baseline is generated and ignored.

Set `BENCH_REVISION=<commit>` for another production revision. `prepare.py`
replaces only the ignored extraction and records the full source commit. The build
adapts to the revision's public engine, snapshot-host and invocation-context APIs;
it does not patch the extracted implementation. Original deployment APIs remain
reproducible after the public wrapper is removed in newer revisions. New summaries
include `baseline_revision`. Context-aware cells use a fixed timestamp and seed;
the current query/sync fixtures do not call time or randomness.

## Changes since RESULTS.md

The fixture gained a `bump` mutation and query arguments (tiny bundle is now 681
bytes, large 143,077), the direct bootstrap exposes `db.put`, workers select the
export per call and return dependencies/writes, and a `sync` workload runs the
single-writer sync engine in `src/sync.rs`. `BENCH_WARMUP`/`BENCH_CALLS` override
sample counts. The Bun comparison and re-run Rust cells are in
[../bun/RESULTS.md](../bun/RESULTS.md); `../report.py` renders both result sets.

## Run

From the repository root:

```sh
python3 crates/chunk-js/benches/engine/prepare.py
cargo build --release --locked --manifest-path crates/chunk-js/benches/engine/Cargo.toml
python3 crates/chunk-js/benches/engine/run.py unrestricted
python3 crates/chunk-js/benches/engine/run.py quota
python3 crates/chunk-js/benches/engine/run.py unrestricted --engines isolate context cold cached terminate --workloads empty --sizes 128 --name primitives
```

The runners require Linux and a systemd user manager with delegated CPU/memory
controllers. They run cells sequentially, restrict affinity to CPUs 0 and 1, and
limit memory to 512 MiB. The quota profile sets `CPUQuota=12.5%` and
`CPUQuotaPeriodSec=80ms`, then verifies `cpu.max` is `10000 80000`. This is the
aggregate quota of a Fly `shared-cpu-2x` without burst credits.

Individual cells:

```sh
crates/chunk-js/benches/engine/target/release/chunk-js-engine-bench fresh query 128 10
BENCH_CACHE=evaluate crates/chunk-js/benches/engine/target/release/chunk-js-engine-bench fresh query 128
BENCH_INIT=declarations crates/chunk-js/benches/engine/target/release/chunk-js-engine-bench fresh query 128
crates/chunk-js/benches/engine/target/release/chunk-js-engine-bench terminate empty 0
```

Arguments: engine (`deno`, `fresh`, `persistent`, `isolate`, `context`, `cold`,
`cached`, `terminate`), workload (`empty`, `reads`, `query`, `sync`), target bundle KiB,
and optional calls arriving together every 80 ms. Zero means closed-loop maximum
throughput. The burst schedule never skips late arrivals: reported response times
include the backlog when offered work exceeds capacity.

For a temporary Fly Machine, upload the executable and `fly-quota.sh` to `/root`
with mode 0755, then run `/root/fly-quota.sh fresh query 128 10`. The script places
the benchmark and all its threads in a guest cgroup with the same aggregate
10 ms / 80 ms limit. Host burst credits cannot bypass that guest limit. Fly's own
scheduler and other VM processes can still add delay. The script creates no
network listener. Remove the temporary Machine and app when finished.

## What is measured

- Every cell has 1,000 warm-up calls and 10,000 measured calls. Returned values are
  checked against independently calculated results for seven changing snapshots.
- `empty` returns null. `reads` returns a Promise and sums ten point reads. `query`
  returns a Promise, scans twenty records, sorts a leaderboard and returns ten
  records. Its snapshot changes on each call; the host records points and ranges.
  This models the evaluation portion of a subscribed query, not invalidation or
  subscriber delivery.
- The tiny bundle is the fixture alone. The larger synthetic bundle adds roughly
  128 KiB of function declarations, a registry of those functions, and calls every
  function at module initialization. Actual bytes exceed the target because the
  registry follows the declarations. `BENCH_INIT=declarations` keeps the registry
  but removes eager initialization, isolating that cost. Neither is an application
  capacity benchmark.
- The direct worker paths use a dedicated worker thread, a channel round trip, a
  persistent watchdog, a 32 MiB V8 heap limit, a fresh caller/argument conversion,
  microtask checkpoints, V8 foreground-task draining for GC/JIT work, and strict
  JSON result serialization. The host snapshot
  is constructed inside each measured call for both engines.
  The direct `actor` path instead shares the orchestration thread. Production
  `deno` revisions use their own lifecycle: a worker at #21/#30, caller-thread
  Engine at #31 and later. Snapshot-host adapters return raw get/scan rows when
  the production API owns invocation-overlay merging.
- `fresh` creates and drops context/function handles per invocation, consuming a
  deployment code cache and asserting that V8 accepts it. `persistent` retains
  both the context and initialized module. By default the cache is produced after
  instantiation, before evaluation; `BENCH_CACHE=evaluate` produces it after the
  first module evaluation, capturing code compiled during top-level execution.
- Per-call wall latency includes the worker round trip and handle cleanup.
  Process CPU includes the caller, worker, watchdog, and V8 background threads.
  Overall CPU/call includes measurement and value checking. RSS is the process
  high-water mark, including warm-up. It is not total VM memory.
- `context_us`, `bootstrap_us`, and `module_us` attribute time inside fresh calls.
  Allocation/GC may be charged to whichever phase triggered it; phase p99 values
  must not be added. `module_us` includes compilation/cache consumption,
  instantiation, evaluation, and export lookup.
- `isolate` measures creation plus disposal. `context` measures context creation
  and handle-scope exit in one reused isolate. `cold` loads into a new isolate on
  every iteration, so wall time includes isolate/bootstrap/cache production;
  `module_us` isolates the module portion. `cached` measures module load/evaluate
  from serialized cache in a reused context, without bootstrap or handler calls.
- `terminate` requests termination after JS signals entry into a synchronous
  loop. `termination_us` measures request-to-worker-acknowledgment; whole-call
  wall time also includes creation and setup. Each terminated isolate is dropped.

`results/*.jsonl` contains raw cell summaries including guest cgroup throttle
counters. In cgroup v1, `throttled_time` is nanoseconds; in v2, `throttled_usec` is
microseconds. These counters describe the benchmark cgroup, not Fly host steal
time. Process CPU timings are CPU consumption, not elapsed time under throttling.

Recorded results are in [RESULTS.md](RESULTS.md), with retained JSON summaries in
`evidence/`. The initial direct-engine measurements were superseded after adding
V8 foreground-task draining; only the updated direct measurements are retained in
the evidence. The uncached load measurement uses fresh isolates to exclude V8's
implicit compilation cache.

## Limits of the prototype

The bootstrap supplies a context factory, frozen global object, basic deterministic
time/random placeholders and a strict serializer. It does **not** implement the
proposed web API subset, full seed handling, hardened capability limits, mutation
overlay, near-heap recycling, unhandled-rejection handling, or the async deployment
API. These results establish a performance floor and architectural tradeoffs;
the finished engine must be measured again with those features and real bundles.

The `97f0ee1` baseline retains its Tokio runtime and V8 deployment, but creates a
watchdog per call. `a124f1a` results used the earlier per-call-runtime lifecycle.
Later revisions retain the watchdog and remove the deployment worker handoff.
Each table names its exact source revision. The direct persistent-context result is a performance baseline,
not an endorsement of persistent mutable state for transactional calls.

All JS fixture/bootstrap code here is original project code. No third-party JS
polyfills are vendored by this experiment.
