# chunk-bench

Opt-in local benchmarks for the gateway's relay and for core (control and the backend) over its `chunk.sync.v1`
protocol. They build in release mode and never run in tests or CI.

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
just bench sync-queries --subscribers 200 --rate 100 --writes unrelated
just bench sync-queries --subscribers 5000 --rate 100 --writes related --own-writes
```

`just bench --help` lists every parameter. By default a run warms up for 2 seconds and measures for 10, over 64
connections or RPC lanes, with two generator threads and as many target threads as the machine has (`--target-threads`
overrides it). Nothing is pinned unless you pass `--target-cpus 12-13`, which runs the target under `taskset -c` (set
`--target-threads` to match; pin the generator yourself). Run benchmarks one at a time on a quiet machine, repeat each
point, and compare only runs with the same build, payload, duration and hardware.

The backend workloads compile the TypeScript app in `backend/`, which needs the pinned TypeScript beside the release
build: run `node scripts/install-typescript.mjs target/release` once, or set `CHUNK_TYPESCRIPT`.

## Workloads

| Workload             | One operation                                                                        | Included work                                                                                                          |
| -------------------- | ------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------- |
| `proxy-relay`        | Client packet → relay → synthetic gameplay response → relay → client                 | The gateway's play-state relay, framing, AES-128-CFB8 encryption and zlib compression over real TCP                    |
| `control-population` | Subscribe to `gateway/<id>` and read its snapshot of every seeded claim              | Sync authentication, stream fencing, control state reads, normal reconciliation, arrival and health tasks              |
| `control-churn`      | `chunk:claim` → `chunk:activate` → ARRIVED on `gateway/<id>` → `chunk:depart`        | Sync authentication, fenced calls, placement, JVM reports, topic delivery, durable SQLite commits and background tasks |
| `backend-query`      | Load one player's profile through a `by_player` index, as a sync `Call` from the CLI | Sync authentication, engine queue, JS evaluation, SQLite snapshot reads, contract validation                           |
| `backend-mutation`   | Save one player's profile (load, patch, return save count)                           | As above, plus retry-context preparation and a durable SQLite commit before the reply                                  |
| `sync-queries`       | One app mutation through sync `Call` while `queries` streams follow                  | Core's backend and control, sync authentication, position-only advances or reevaluation, and stream delivery           |

**`proxy-relay`** uses pre-established connections, so it excludes Mojang login, configuration, commands, admission and
moves. Each request and response is checked byte for byte, and each connection has one round trip outstanding. Bodies
default to 32 bytes up and 1 KiB down with mixed compressibility; `--payload repeated|mixed|random` changes that,
`--payload chunk` sends a synthetic overworld chunk column down instead, and `--no-compression`, `--no-encryption` and
`--compression-level` separate the costs. `--burst N` answers each request with N packets in one write. The summary's
`response` reports body and framed sizes. These opaque packets model transport work, not a real play session: there is
no server broadcast, slow reader or backpressure workload.

**Control workloads** run core in the target with its normal background tasks and on-disk SQLite, and call it as core's
own gateway. Synthetic JVMs, one per host, follow their `jvm/<host>` topics inside the target process, so their work
counts toward the target; they report sessions and arrivals but launch no JVMs and simulate no ticks or failures. The
fixture declares 128-player sessions, one session per process and at most 32 processes. `control-population` offers 100
snapshots per second by default, each what a reconnecting gateway reads. `control-churn` reports complete player
lifecycles per second, and `population + concurrency` may be at most 1024. Released claims are kept for five minutes, so
shorter runs measure churn with growing history. Setup and a successful run end by checking that a fresh snapshot holds
exactly the seeded population.

**Backend workloads** compile `backend/` with `chunk_build::compile` and serve it from core's backend on on-disk SQLite.
Its schema has a `profiles` table, with `by_player` and `by_rank` indexes, and an `activity` table. Setup seeds
`--population` profiles, 200 per commit. Each lane is its own connection calling as the CLI; operation IDs are unique,
so no request recovers a stored outcome. The target builds `chunk-backend` with `bench-support`, which records phase
timings after warmup: `queue`, `query`, `mutation`, `prepare`, `commit`, `durable`, `reevaluate` and `fanout`. Phases
overlap; don't add or subtract their percentiles.

**`sync-queries`** opens `--subscribers` (default 5,000) `queries` streams on a shared leaderboard,
`--streams-per-connection` to a connection, and writes through sync `Call`s from `--concurrency` lanes.
`--writes unrelated` (the default) writes to a table no query reads, so streams only advance their position, promptly
under `--own-writes`; `--writes related` changes every stream's result. `--slow-readers` streams wait `--slow-read-ms`
before each read, on their own connections, and `--result-padding` enlarges the result to press on core's send budget.
The `fanout` summary reports stream updates, changed entries, encoded bytes, errors, streams ended or overloaded, and
`reply_to_observed_us`, from a write's reply to each prompt stream observing it; pairs still unobserved two seconds
after the last write are `unobserved_pairs`.

`cargo bench -p chunk-js --bench engines -- target/bench/<run>/bundle` compares chunk-js's engine with a plain V8
context on a backend workload's compiled bundle.

## Measurement and output

The target runs in a child process; clients and synthetic services run in the parent, and their CPU and memory are
reported separately. Everything binds ephemeral loopback ports and temporary state, and completion, errors and Ctrl-C
close it all.

Requests follow an absolute schedule, independent of replies. If every lane is busy, the offer counts as `dropped_busy`;
offers that miss their deadline or the load window are `dropped_late`. Latency starts at the scheduled send time. A
timeout counts as a failure, and no histogram correction is applied.
`offered = completed + failed + dropped_busy + dropped_late` always holds. CPU is process CPU time over wall time (1.0
is one core), sampled about once a second, including drain and excluding setup and warmup. Proxy bandwidth counts
delivered uncompressed bodies once; it is not wire bandwidth.

Each run writes to `target/bench/<timestamp>-<pid>/`:

- `config.json`: the arguments, commit and dirty state, toolchain, CPU, OS and memory.
- `summary.json`: counts, error categories, latency and scheduler-delay percentiles, throughput and resource samples.
- `warmup-*.hdr` and `measured-*.hdr`: HdrHistogram V2 data in microseconds (`latency`, `success`, `scheduler`), plus
  `measured-phase-<name>-ns.hdr` for backend phases and `measured-observed.hdr` for `sync-queries`.
- `bundle/`, for backend workloads: the compiled `source.mjs`, its source map, `contract.json` and `deployment.json`.
- `failure.json`, if setup, warmup or shutdown fails. A measured overload can still exit successfully; read the failure
  and drop counts, not just the exit status.

These are local baselines, not hosting or player-capacity figures. The runner provisions nothing and never connects to
existing environments.

## Layout

`load.rs` owns pacing and accounting, `metrics.rs` histograms, `fixtures.rs` and `proxy.rs` the synthetic services, and
`backend/` the benchmark app; workload modules call production APIs. Keep benchmark dependencies in this unpublished
crate and results out of source control.
