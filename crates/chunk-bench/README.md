# Service benchmarks

Opt-in local workloads for the production proxy relay, control server and environment backend. Use a release build;
these workloads never run as part of tests or CI.

```sh
just bench proxy-relay
just bench proxy-relay --rate 10000 --concurrency 128 --seconds 30
just bench proxy-relay --response-bytes 32768 --payload random --rate 1000
just bench proxy-relay --burst 16 --response-bytes 256 --rate 2000
just bench control-population --population 128 --seconds 30
just bench control-population --population 512 --rate 2000 --seconds 30
just bench control-churn --population 128 --rate 20 --seconds 20
just bench backend-query --population 5000 --rate 2000
just bench backend-mutation --population 5000 --rate 100 --concurrency 16
just bench backend-fanout --population 5000 --rate 2 --subscribers 1024 --group-size 16 --subscription shared
```

`just bench --help` lists the parameters. Defaults are 2 seconds of warmup, 10 seconds of measurement, 64
connections/RPC lanes, and two Tokio worker threads each for target and generator. No CPU affinity or resource quotas
are applied unless you pass `--target-cpus 12-13` (runs the target under `taskset -c`) and start the generator under
`taskset` yourself; `config.json` records the arguments. Run benchmarks sequentially on an otherwise quiet machine and
repeat each point. Use the same build, payload, duration and hardware when comparing results.

## Workloads

| Workload             | One operation                                                        | Included work                                                                                                    |
| -------------------- | -------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| `proxy-relay`        | Client packet → relay → synthetic gameplay response → relay → client | Production managed PLAY relay, framing, AES-128-CFB8 encryption and zlib compression over real TCP               |
| `control-population` | `PollMove` for an arrived player without a pending move              | Real authenticated gRPC, durable state reads, normal reconciliation and health tasks                             |
| `control-churn`      | New `Claim` → `Activate` → `ReconcileDeparture`                      | Real authenticated gRPC, placement, runtime RPCs, ownership changes, durable SQLite commits and background tasks |
| `backend-query`      | Load one player's profile through a `by_player` index                | Real authenticated backend gRPC, engine queue, JS evaluation, SQLite snapshot reads, contract validation         |
| `backend-mutation`   | Save one player's profile (load, patch, return save count)           | As above, plus retry-context preparation and durable SQLite commit before the reply                              |
| `backend-fanout`     | Raise one player's best score, then wait until every stream has it   | As above, plus reevaluation of every subscription and delivery over each watch stream                            |

The proxy uses pre-established connections. It excludes Mojang login, configuration, command handling, admission,
movement between servers and control polling. A feature-gated adapter calls the existing packet pump; normal proxy
listeners and authentication policy are unchanged. Each request and response is checked byte-for-byte, including its
sequence number. Default packet bodies are 32 bytes upstream and 1 KiB downstream with mixed compressibility. Try
`--payload repeated`, `mixed` and `random`, and `--no-compression` / `--no-encryption` to distinguish costs. These
opaque packets model transport work, not an actual Minecraft play session or representative traffic capture. Each
connection has at most one outstanding round trip. `--burst N` makes the gameplay server answer each request with N
response packets in one write, which exercises write batching; there is still no independent server broadcast, sustained
one-way stream, slow reader or backpressure workload.

Control populations are seeded through real claim and activation RPCs. Synthetic runtimes provide independent process
identities, session inventories and instant player arrival; they do not launch JVMs or simulate game ticks, startup or
network failures. The target uses `chunk_control::server::run`, including its normal background tasks and on-disk SQLite
settings. The fixture declares 128-player sessions, one session per process, and at most 32 processes.

Population defaults to two polls per player per second, approximating the proxy's 500 ms move polling cadence. This is
deliberately an open-loop offered rate; production polling waits for each reply, so it self-throttles under overload.
Churn reports complete player lifecycles per second, not individual RPCs. Control holds at most 1,024 open claims, so
the runner rejects churn configurations where `population + concurrency > 1024`. Released claims are retained for five
minutes, so runs shorter than that measure churn with growing history. Every run starts with fresh state. Setup reports
the player index on failure; target warnings and errors go to stderr. Treat capacity rejections separately from
throughput saturation.

Backend workloads compile the TypeScript app in `backend/` with `chunk_build::compile`, exactly as an app is built, and
serve it with `chunk_backend::server::run` on on-disk SQLite. Compilation needs the pinned native TypeScript: run
`node scripts/install-typescript.mjs target/release` or set `CHUNK_TYPESCRIPT`. The bundle has one `profiles` table (ten
fields including an eight- to sixteen-item inventory) with `by_player` and `by_rank` indexes; `rank` is the negated best
score because indexes are ascending. Setup seeds `--population` profiles through the bundle's own `seed` mutation, 200
per commit. Each lane is its own connection with a fixed player caller. Operation IDs are unique, so no request recovers
a stored outcome.

`backend-fanout` opens `--subscribers` query subscriptions in watch groups of `--group-size`, one connection per group,
and waits for every initial result. `--subscription shared` subscribes everyone to the identical leaderboard (`top`);
`per-player` subscribes everyone to their own standing (leaderboard plus their profile). Every write reads the current
leader and sets a higher score, so it changes every subscriber's result in any arrival order. An operation completes
only when every stream has delivered that write's revision or a later one; slow streams coalesce. The backend admits 64
subscriptions in total, each a group of at most 16 queries, so at most 1,024 subscribers fit. Larger requests fail
during setup with the backend's own rejection. Admission also allows 16 outstanding mutations and 64 outstanding
requests; all capacity rejections report the same `ResourceExhausted` status.

Backend targets build `chunk-backend` with its `bench-support` feature, which exposes phase timings from the engine and
commit threads without changing behavior. Phases are recorded after warmup: `queue` (admission until the engine thread
takes a query or mutation), `query` and `mutation` (evaluation and validation), `prepare` and `commit` (SQLite work on
the commit thread), `durable` (staged until the engine handles the commit acknowledgment), `reevaluate` (one subscribed
query; identical subscriptions share it) and `fanout` (commit acknowledgment until the batch covering it reevaluated
every affected query; commits coalesced into one batch each report their wait). Phases overlap; do not add or subtract
their percentiles. `fanout.reply_to_all_delivered_us` is measured by the generator from the durable mutation reply to
the last stream's delivery. `target_cpu_us_per_completed` divides target CPU, including drain, by completed operations.

`cargo bench -p chunk-js --bench engines -- target/bench/<run>/bundle` compares chunk-js's persistent `deno_core` engine
with a persistent direct V8 context on the compiled bundle. Both get the same in-memory snapshot of 1,000 profiles, a 32
MiB heap and a one-second watchdog. Each cell runs 1,000 warmup and 8,000 measured calls (below chunk-js's 10,000-call
runtime recycling) in its own process and checks that both engines return identical results.

## Measurement and output

The target runs in a child process. Clients and synthetic gameplay/runtime services run in the parent, so their CPU and
memory are reported separately. All listeners bind ephemeral loopback ports. State is temporary, and normal completion,
errors and Ctrl-C close the owned services and child process.

Requests follow an absolute schedule, independent of prior replies. A free lane is chosen in FIFO order; if all lanes
are busy, that offer is counted as `dropped_busy` rather than queued. Offers missing their deadline or the load window
are `dropped_late`. Latency starts at the **scheduled** send time, including generator delay. An in-flight timeout is
counted as a failure; a timed-out relay connection is discarded because its framing/cipher state cannot safely be
reused. Accepted control operations can continue in the target after a client timeout, as they do in production. No
synthetic histogram correction is applied.

Each run writes to `target/bench/<timestamp>-<pid>/` (gitignored):

- `config.json`: all arguments, resolved rate, commit/dirty state, release mode, toolchain, CPU, OS and memory.
- `summary.json`: warmup and measured counts, error categories, latency and scheduler-delay percentiles, throughput, and
  roughly one-second process samples.
- `warmup-*.hdr` / `measured-*.hdr`: mergeable HdrHistogram V2 data in microseconds, with three significant digits.
  `latency` includes completed successes and failures; `success` contains only successful completions; `scheduler`
  covers offers examined during the window. Unsent offers have no completion latency.
- `measured-phase-<name>-ns.hdr` (backend target phases, nanoseconds) and `measured-delivery.hdr` (fan-out reply to last
  delivery, microseconds).
- `bundle/` for backend workloads: the compiled `source.mjs`, `contract.json` and `deployment.json`.
- `failure.json` if setup, infrastructure, warmup or shutdown fails. A measured overload can still finish successfully:
  inspect failure/drop counts, not just the command's exit status.

`offered = completed + failed + dropped_busy + dropped_late` must always hold. Throughput is provided both per
offered-load second and per elapsed second including the final drain. Proxy bandwidth counts successfully delivered
uncompressed request + response bodies once; it is **not wire bandwidth**. CPU is cumulative process CPU time divided by
elapsed wall time (1.0 = one full core); RSS samples can miss short peaks. Resource sampling includes drain and excludes
setup/warmup. The generator resource numbers include fixture and sampling overhead. A growing generator delay or
saturated generator can invalidate a target-capacity conclusion.

These are local implementation baselines, not Fly sizing or player-capacity claims. Test the intended Fly machine sizes,
quotas, regions, disks and network separately before using results for hosting decisions. This runner does not provision
infrastructure or connect to existing environments.

## Keeping it small

`load.rs` owns pacing and accounting; `metrics.rs` owns histograms; workload modules call production APIs. Synthetic
services stay in `fixtures.rs` and `proxy.rs`; the benchmark app stays in `backend/`. Keep benchmark dependencies in
this unpublished crate and raw results out of source control. Add workload dimensions when answering a concrete
question; add Criterion microbenchmarks only when a measured hotspot warrants isolation.
