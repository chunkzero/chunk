# Embedded JS engine measurements — 2026-09-07

Fresh contexts improve substantially on the unchanged #21 implementation, but
this prototype does not establish a sub-millisecond p99 guarantee on the target
Fly machine. CPU cost per invocation and burst size determine whether work fits
inside the shared CPU quota.

All cells have 1,000 warm-up calls and 10,000 measured calls, with result checks.
See [methodology and reproduction](README.md) and [machine/toolchain metadata](evidence/machine.json).
The direct prototype retains an isolate, source text and compiled-code cache;
it creates a context, loads the bootstrap, consumes the module cache and executes
module initialization on each fresh call. It does not restore a deployment heap
snapshot. Storage is represented by a small in-memory host, not a database.

## Local comparison

Ryzen 7 7840HS, affinity restricted to two logical CPUs, 512 MiB process-group
limit, no CPU quota. Both paths use V8 150.4.0; baseline is the unchanged #21
engine at `a124f1a`. Each cell is a separate process, run sequentially.
The large fixture includes eager initialization of roughly 1,300 helper functions.

| Bundle bytes | Workload | Engine | Wall median ms | Wall p99 ms | Mean process CPU ms/call | Peak RSS MiB |
| ---: | --- | --- | ---: | ---: | ---: | ---: |
| 452 | empty | deno | 4.142 | 4.697 | 4.543 | 29.2 |
| 452 | empty | fresh | 0.221 | 0.656 | 0.247 | 30.7 |
| 452 | empty | persistent | 0.021 | 0.032 | 0.023 | 26.5 |
| 452 | query | deno | 4.203 | 4.857 | 4.606 | 29.8 |
| 452 | query | fresh | 0.274 | 0.735 | 0.296 | 32.9 |
| 452 | query | persistent | 0.044 | 0.062 | 0.047 | 39.0 |
| 452 | reads | deno | 4.186 | 4.480 | 4.567 | 29.1 |
| 452 | reads | fresh | 0.272 | 0.806 | 0.304 | 33.3 |
| 452 | reads | persistent | 0.033 | 0.049 | 0.035 | 37.9 |
| 143,154 | empty | deno | 10.817 | 11.626 | 11.302 | 31.3 |
| 143,154 | empty | fresh | 0.749 | 1.653 | 0.849 | 39.4 |
| 143,154 | empty | persistent | 0.020 | 0.025 | 0.021 | 28.7 |
| 143,154 | query | deno | 11.096 | 11.973 | 11.685 | 32.5 |
| 143,154 | query | fresh | 0.801 | 1.715 | 0.927 | 39.3 |
| 143,154 | query | persistent | 0.045 | 0.061 | 0.048 | 40.0 |
| 143,154 | reads | deno | 10.879 | 11.782 | 11.367 | 32.1 |
| 143,154 | reads | fresh | 0.773 | 1.671 | 0.887 | 39.2 |
| 143,154 | reads | persistent | 0.033 | 0.049 | 0.035 | 38.9 |

Raw summaries: [local.jsonl](evidence/local.jsonl).

## Component measurements

Local machine, same sample count. Creation measurements include handle cleanup
and naturally occurring GC. The cold module row is the module phase of a fresh
isolate load; full cold load also includes isolate/context/bootstrap setup and
cache production. The cached row consumes serialized cache in a reused context
and evaluates the large eager bundle. Those scopes are intentionally explicit;
they are not subtraction-based estimates.

| Operation | Median ms | p99 ms |
| --- | ---: | ---: |
| Isolate creation + disposal | 0.404 | 0.521 |
| Context creation + cleanup | 0.151 | 0.985 |
| Full cold isolate/context/bundle load | 7.706 | 8.210 |
| Cold module phase, within the preceding load | 6.961 | 7.337 |
| Cached bundle instantiation + evaluation | 0.496 | 0.711 |
| Direct termination request → worker acknowledgment | 0.007 | 0.011 |

Every termination sample used a new isolate. Request latency excludes isolate
recreation; complete create/run/terminate iterations took 0.785 ms median and
0.943 ms p99. These termination figures are local and unthrottled; CPU quota can
delay both the requester and worker. Raw summaries: [primitives.jsonl](evidence/primitives.jsonl).

## Bundle initialization and cache timing

For the large fresh-context query on the local machine:

| Variant | Wall median ms | Wall p99 ms | Mean process CPU ms/call |
| --- | ---: | ---: | ---: |
| Eager initialization, cache after instantiation | 0.801 | 1.715 | 0.927 |
| Eager initialization, cache after evaluation | 0.805 | 1.751 | 0.939 |
| Declarations + registry, no eager initialization | 0.457 | 1.176 | 0.518 |

Moving cache production after evaluation made no material improvement in these
warm runs. Removing eager initialization reduced CPU cost substantially, but
module instantiation still costs time. Raw summaries: [diagnostics.jsonl](evidence/diagnostics.jsonl).

## Fly with the sustained quota enforced

Temporary Fly Machine in `iad`: two shared vCPUs, 512 MiB, guest-reported AMD
EPYC (family 25/model 1), Debian trixie. A guest cgroup enforced **10,000 us CPU
per 80,000 us period across all benchmark threads**. Every cell verified that
quota. This removes dependence on host burst credits; Fly host scheduling and
other VM work can still add delay. These are on-machine timings, excluding WAN.
The Machine and temporary app were removed after measurement.

All rows below use the query workload. The large bundle retains declarations
and their registry but omits eager helper initialization. All source and code
cache bytes are already in memory. No deployment heap snapshot is restored.

| Bundle bytes | Engine | Arrivals per 80 ms | Offered calls/s | Wall median ms | Wall p99 ms | Mean process CPU ms/call | Response p99 ms | Max response ms |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 452 | fresh | 10 | 125.0 | 0.628 | 1.701 | 0.738 | 8.618 | 76.768 |
| 452 | persistent | 10 | 125.0 | 0.141 | 0.380 | 0.174 | 2.167 | 3.935 |
| 143,012 | fresh | 5 | 62.5 | 1.051 | 2.566 | 1.244 | 7.881 | 79.175 |
| 143,012 | fresh | 10 | 125.0 | 0.978 | 73.246 | 1.161 | 13357.258 | 13515.602 |
| 143,012 | persistent | 10 | 125.0 | 0.145 | 0.373 | 0.176 | 2.117 | 6.189 |

Response time starts at the scheduled arrival of each batch. When offered work
exceeds capacity, the harness preserves the scheduled timestamps and drains the
logical backlog; it does not skip arrivals. The roughly 13-second p99 is queue
backlog, not a single handler executing for 13 seconds. A bounded production
queue would need to reject or defer work before reaching that state.

For the tiny fresh-context query, measured cgroup throttling occurred in 27 of
999 measured quota periods. Its maximum individual call latency was 70.748 ms,
and maximum scheduled response was 76.768 ms. Thus even its sub-millisecond
median is not a hard latency bound. Initial warm-up throttle counts are excluded
from that delta. The test intentionally includes GC and scheduling tails.

Raw summaries, including throttle counters and RSS: [fly.jsonl](evidence/fly.jsonl).

## Budget implications

Fly documents the quota as 10 ms **total**, shared by the two vCPUs, every 80 ms:
[CPU performance](https://fly.io/docs/machines/cpu-performance/). That is 125 ms of
sustained CPU per second for the entire VM. The benchmark cgroup grants that
whole amount to the experiment; the real backend must also pay for storage,
validation, subscription management, transport and other runtime work.

- Tiny fresh-context query: 0.738 ms CPU/call. Ten evaluations consume about
  **7.38 ms**, before the remaining backend costs. At 125 calls/s, this consumes
  about **74%** of the VM's sustained budget.
- Large declarations-only fresh-context query at 125 calls/s: 1.161 ms CPU/call,
  or **145 ms CPU/s**, exceeding the 125 ms/s quota. Its measured completion rate
  fell below the offered rate and the backlog grew throughout the run.
- At 62.5 calls/s, the large fresh-context query used 1.244 ms CPU/call, about
  **78 ms CPU/s**, leaving roughly 47 ms CPU/s for everything else. Individual
  call p99 was still 2.567 ms.
- The identical large bundle in a persistent context at 125 calls/s used about
  **0.176 ms CPU/call**, or **22 ms CPU/s**. That is roughly **6.6 times less CPU**
  than the fresh-context case at the same offered rate.

These rates include every evaluation: a mutation that triggers ten distinct
subscription evaluations spends ten calls' worth of CPU, not one. The simple
formula is `evaluations/s * mean CPU ms/evaluation <= 125 - other VM CPU ms/s`.
It is an average capacity condition, not a tail-latency guarantee.

## Interpretation and remaining work

The existing #21 engine misses the sub-millisecond target by a wide margin.
Reusing V8 isolates makes fresh-context execution much cheaper, but this
prototype does **not** meet a strict sub-millisecond p99 target on Fly. Even a
small fixture has occasional quota stalls, and the larger fixture can exceed
sustained capacity at modest evaluation rates. A persistent context has a real
performance advantage, with the module/global-state correctness tradeoff
explained in D36.

Before committing to the engine PR's latency expectation, measure a representative
application bundle and consider a startup/deployment snapshot experiment to reduce
repeated initialization while preserving invocation isolation. This experiment
has not measured that alternative. Include the full web API bootstrap, backend
storage/validation and subscription fanout before setting a supported workload
limit. The fixed web API subset and other production hardening are absent here;
these numbers must not be treated as final engine or whole-backend capacity.

Validation: release build, focused Clippy with warnings denied, Rust formatting,
Python syntax, baseline source identity, cache-acceptance checks and result checks
across all reported invocation cells. Termination was verified with V8 TryCatch's
termination flag and each terminated isolate was discarded. No production code,
PRs, or stacked branches were changed by this experiment.

## Boundary optimizations — 2026-09-07 (later run)

Three variants of the persistent design, all with the 681-byte and 143 KB
bundles, same machine and cgroup as above, run back to back so the `persistent`
rows here are the fair baseline (they are slightly slower than the morning run).

- `persistent`: unchanged worker thread, channel per call, watchdog woken twice per call.
- `tuned`: same thread hop, but one watchdog thread polling an atomic deadline,
  and host values crossing as JSON text plus primitives instead of `serde_v8` trees.
- `actor`: sync engine and isolate on the same thread, no channel; same trims.

### Unrestricted, closed loop

| Bundle bytes | Workload | Engine | Calls/s | Wall median ms | Wall p99 ms | Mean process CPU ms/call | Peak RSS MiB |
| ---: | --- | --- | ---: | ---: | ---: | ---: | ---: |
| 681 | query | persistent | 16026 | 0.056 | 0.075 | 0.071 | 38.6 |
| 681 | query | tuned | 16202 | 0.053 | 0.066 | 0.081 | 30.9 |
| 681 | query | actor | 29351 | 0.028 | 0.038 | 0.034 | 30.6 |
| 681 | sync | persistent | 9910 | 0.095 | 0.139 | 0.109 | 41.1 |
| 681 | sync | tuned | 12103 | 0.082 | 0.100 | 0.099 | 33.4 |
| 681 | sync | actor | 24617 | 0.040 | 0.058 | 0.041 | 33.4 |
| 143077 | query | persistent | 16507 | 0.055 | 0.074 | 0.067 | 40.4 |
| 143077 | query | tuned | 16142 | 0.054 | 0.067 | 0.081 | 32.9 |
| 143077 | query | actor | 29607 | 0.028 | 0.041 | 0.034 | 31.8 |
| 143077 | sync | persistent | 10353 | 0.094 | 0.119 | 0.107 | 43.3 |
| 143077 | sync | tuned | 12021 | 0.082 | 0.100 | 0.099 | 35.0 |
| 143077 | sync | actor | 24681 | 0.040 | 0.054 | 0.041 | 34.0 |

### Local 10 ms / 80 ms quota, burst arrivals (large bundle is declarations only)

| Bundle bytes | Workload | Engine | Arrivals per 80 ms | Wall median ms | Wall p99 ms | Mean process CPU ms/call | Response p99 ms | Max response ms |
| ---: | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 681 | query | persistent | 10 | 0.108 | 0.425 | 0.148 | 2.090 | 4.159 |
| 681 | query | tuned | 10 | 0.115 | 0.389 | 0.150 | 2.006 | 2.579 |
| 681 | query | actor | 10 | 0.044 | 0.210 | 0.097 | 1.432 | 2.023 |
| 142935 | query | persistent | 10 | 0.109 | 0.426 | 0.148 | 2.062 | 4.151 |
| 142935 | query | tuned | 10 | 0.118 | 0.378 | 0.151 | 1.997 | 2.364 |
| 142935 | query | actor | 10 | 0.044 | 0.215 | 0.096 | 1.384 | 1.648 |
| 681 | sync | persistent | 5 | 0.186 | 0.619 | 0.260 | 1.851 | 3.236 |
| 681 | sync | tuned | 5 | 0.177 | 0.553 | 0.260 | 1.836 | 72.195 |
| 681 | sync | actor | 5 | 0.061 | 0.310 | 0.152 | 1.164 | 1.435 |
| 142935 | sync | persistent | 5 | 0.183 | 0.634 | 0.260 | 1.860 | 4.824 |
| 142935 | sync | tuned | 5 | 0.166 | 0.567 | 0.259 | 1.767 | 4.951 |
| 142935 | sync | actor | 5 | 0.063 | 0.315 | 0.161 | 1.196 | 1.566 |

The actor roughly doubles throughput and halves CPU per call and p99 against the
worker: the channel hop and its wake-ups were the dominant per-call cost. The
JSON-text interchange on its own was a wash for these small documents
(`serde_json::to_string` per read replaced `serde_v8` construction at similar cost);
it only pays off once rows arrive from storage already serialized. The atomic
deadline removed the watchdog wake-ups, which shows in the tuned rows' lower p99
and RSS but not in mean CPU. Quota CPU/call figures include the harness's
sleep/wake per burst and are comparable only within the table.


## Persistent Deno baseline at PR #21 head `97f0ee1`

The earlier `deno` rows used `a124f1a`, which recreated the runtime per call.
`97f0ee1` uses `Deployment::new` once and executes against a persistent runtime.
The harness now extracts that revision and uses its deployment API. Same local
unrestricted profile, 1,000 warm-up and 10,000 checked calls. Recycling after
10,000 calls remains enabled. Each sync operation performs three evaluations.

| Bundle bytes | Workload | Median µs | p99 µs | Process CPU µs/operation |
| ---: | --- | ---: | ---: | ---: |
| 681 | query | 67.6 | 192.5 | 81.9 |
| 681 | sync | 148.3 | 541.3 | 181.7 |
| 143077 | query | 68.1 | 177.9 | 83.5 |
| 143077 | sync | 148.0 | 525.7 | 183.7 |

Raw summaries: [deno-97f0ee1.jsonl](evidence/deno-97f0ee1.jsonl). These are engine/harness measurements, not integrated storage capacity.

## Combined invocation and persistent watchdog (`c317e26`)

PR #30 combines context creation, handler execution and serialization into one JS
entry, followed by a drain; a persistent watchdog replaces per-call threads.
Inputs cross as JSON text and results remain JSON text. Capabilities are unchanged.
Same unrestricted local profile and checked sample counts as `97f0ee1`.

| Bundle bytes | Workload | Median µs | p99 µs | Process CPU µs/operation |
| ---: | --- | ---: | ---: | ---: |
| 681 | query | 44.0 | 73.9 | 50.8 |
| 681 | sync | 81.4 | 160.3 | 85.8 |
| 143077 | query | 44.5 | 69.2 | 51.9 |
| 143077 | sync | 81.6 | 164.9 | 88.4 |

Raw results: [deno-c317e26.jsonl](evidence/deno-c317e26.jsonl).


## Caller-thread deployment engine (`10acc20`)

PR #31 replaces deployment worker/channel calls with a caller-thread Engine, sharing one executor and watchdog across resident versions.
Same local unrestricted profile: 1,000 warm-up and 10,000 checked operations,
with production recycling enabled. Each sync operation performs three evaluations.

| Bundle bytes | Workload | Median µs | p99 µs | Process CPU µs/operation |
| ---: | --- | ---: | ---: | ---: |
| 681 | query | 30.9 | 48.2 | 38.6 |
| 681 | sync | 49.1 | 140.9 | 55.2 |
| 143077 | query | 30.8 | 49.4 | 39.4 |
| 143077 | sync | 49.2 | 144.1 | 57.4 |

Raw results: [deno-10acc20.jsonl](evidence/deno-10acc20.jsonl).


## Environment backend revision (`e2d92c0`)

PR #32 adds the production backend pipeline. This cell exercises the unchanged JS engine through the harness; it does not time SQLite commits, backend ingress or production subscription scheduling.
Same local unrestricted profile: 1,000 warm-up and 10,000 checked operations,
with production recycling enabled. Each sync operation performs three evaluations.

| Bundle bytes | Workload | Median µs | p99 µs | Process CPU µs/operation |
| ---: | --- | ---: | ---: | ---: |
| 681 | query | 31.1 | 47.8 | 38.6 |
| 681 | sync | 49.4 | 136.1 | 55.5 |
| 143077 | query | 31.3 | 56.4 | 40.2 |
| 143077 | sync | 49.6 | 139.0 | 58.3 |

Raw results: [deno-e2d92c0.jsonl](evidence/deno-e2d92c0.jsonl).


## Reviewed stack — 2026-09-08

All four pushed heads were measured after the review fixes; a second independent
review found no further change needed. Same local unrestricted profile, 1,000
warm-up and 10,000 checked operations per cell, with production recycling enabled.
Each sync operation performs three evaluations. The adapter supports the raw
snapshot ReadHost interface, caller-thread Engine and supplied timestamp/seed;
every result records the full extracted revision. Original historical evidence
above remains tied to its own revision and profile.

The reviewed runtime adds controlled time/randomness and bounded ArrayBuffer
allocation, centralizes JS-side write overlays, and parks the shared watchdog
when idle. #32 additionally canonicalizes input JSON at the caller boundary.
These are combined revisions, not isolated measurements of individual fixes.
#32 here still exercises only the engine through the harness, excluding backend
ingress, SQLite, commit scheduling and production subscription delivery.

| PR / revision | Bundle bytes | Workload | Median µs | p99 µs | Process CPU µs/operation |
| --- | ---: | --- | ---: | ---: | ---: |
| #21 `473c2f3` | 681 | query | 70.2 | 204.9 | 85.6 |
| #21 `473c2f3` | 681 | sync | 148.3 | 541.5 | 183.1 |
| #21 `473c2f3` | 143077 | query | 68.8 | 218.2 | 85.2 |
| #21 `473c2f3` | 143077 | sync | 150.2 | 579.4 | 187.3 |
| #30 `6f79c8c` | 681 | query | 48.1 | 81.7 | 56.1 |
| #30 `6f79c8c` | 681 | sync | 86.8 | 174.5 | 97.7 |
| #30 `6f79c8c` | 143077 | query | 48.6 | 77.2 | 57.5 |
| #30 `6f79c8c` | 143077 | sync | 86.8 | 180.9 | 100.7 |
| #31 `0f988f9` | 681 | query | 36.1 | 54.2 | 45.4 |
| #31 `0f988f9` | 681 | sync | 57.2 | 141.0 | 68.9 |
| #31 `0f988f9` | 143077 | query | 36.1 | 64.9 | 46.1 |
| #31 `0f988f9` | 143077 | sync | 56.3 | 136.6 | 69.4 |
| #32 `cd7be82` | 681 | query | 35.6 | 56.5 | 44.8 |
| #32 `cd7be82` | 681 | sync | 56.4 | 132.9 | 67.0 |
| #32 `cd7be82` | 143077 | query | 35.6 | 52.5 | 45.5 |
| #32 `cd7be82` | 143077 | sync | 55.3 | 132.9 | 68.1 |

Raw summaries: [review-473c2f3.jsonl](evidence/review-473c2f3.jsonl), [review-6f79c8c.jsonl](evidence/review-6f79c8c.jsonl), [review-0f988f9.jsonl](evidence/review-0f988f9.jsonl), [review-cd7be82.jsonl](evidence/review-cd7be82.jsonl).
