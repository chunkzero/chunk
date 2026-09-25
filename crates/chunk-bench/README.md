# Service benchmarks

Opt-in local workloads for the production proxy relay and control server. Use a release build; these workloads never run
as part of tests or CI.

```sh
just bench proxy-relay
just bench proxy-relay --rate 10000 --concurrency 128 --seconds 30
just bench proxy-relay --response-bytes 32768 --payload random --rate 1000
just bench proxy-relay --burst 16 --response-bytes 256 --rate 2000
just bench control-population --population 128 --seconds 30
just bench control-population --population 512 --rate 2000 --seconds 30
just bench control-churn --population 128 --rate 20 --seconds 20
```

`just bench --help` lists the parameters. Defaults are 2 seconds of warmup, 10 seconds of measurement, 64
connections/RPC lanes, and two Tokio worker threads each for target and generator. No CPU affinity or resource quotas
are applied. Run benchmarks sequentially on an otherwise quiet machine and repeat each point. Use the same build,
payload, duration and hardware when comparing results.

## Workloads

| Workload             | One operation                                                        | Included work                                                                                                    |
| -------------------- | -------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| `proxy-relay`        | Client packet → relay → synthetic gameplay response → relay → client | Production managed PLAY relay, framing, AES-128-CFB8 encryption and zlib compression over real TCP               |
| `control-population` | `PollMove` for an arrived player without a pending move              | Real authenticated gRPC, durable state reads, normal reconciliation and health tasks                             |
| `control-churn`      | New `Claim` → `Activate` → `ReconcileDeparture`                      | Real authenticated gRPC, placement, runtime RPCs, ownership changes, durable SQLite commits and background tasks |

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
services stay in `fixtures.rs` and `proxy.rs`. Keep benchmark dependencies in this unpublished crate and raw results out
of source control. Add workload dimensions when answering a concrete question; add Criterion microbenchmarks only when a
measured hotspot warrants isolation.
