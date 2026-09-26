# Local control

`chunk-control` is an environment-configured service; `chunk_control::server::run` is its embeddable entry point. Build
with `cargo build -p chunk-control`. Required environment variables:

- `CHUNK_STATE`, `CHUNK_CONNECTION`: durable state directory and discovery record.
- `CHUNK_CONFIG`: JSON `chunk_control::Config` with deployment, external app manifests, profiles and session types.
- `CHUNK_DISTRIBUTION`, `CHUNK_JAVA`: release directory and Java 25 executable.
- `CHUNK_BACKEND_FILE`: backend connection bound to the same deployment.
- Optional `CHUNK_BIND` (default `127.0.0.1:25567`).

`chunk dev` and standalone control use `ProcessHost` to launch each app with `java -jar`. It owns the child handle and
waits for exit; there is no per-server sidecar. External app manifests supply session type IDs, placement and capacity
requirements, and JAR hashes. Control verifies the selected artifact and sends the JVM session creation instructions;
the JVM resolves factories locally and enforces the supplied capacity. Future container/machine providers implement the
same `Host` boundary.

The private `.chunk/local/control.json` connection file authorizes gRPC calls; under `chunk dev` it names the control of
the current deployment version. `Claim` accepts authenticated identity, proxy incarnation, connection identity and a
session demand key/type/profile. It reserves capacity, starts an app JVM if needed, waits for session readiness, and
returns configuration plus a single-use TCP capability. The proxy records admission intent with `Activate` and opens the
native Minecraft connection using the capability. `Inspect` reconciles the same binding; `Cancel` withdraws it before
releasing the reservation. Runtime credentials remain inside control.

Concurrent demand shares compatible sessions up to declared capacity. JVM placement matches both app and machine
profile. A prepared slot is a reservation, not a second attached player. A claim's generation is the `(epoch, revision)`
of the commit that created it; its membership generation is that of the login it continues. Generations compare as
pairs, because a restore starts a new epoch and may reuse revisions. The current wire contract carries a pair as one
`uint64`, the epoch above 40 revision bits. The session and process have their own incarnations. Duplicate login is
rejected while an earlier owner remains unresolved. An old cancellation cannot release a newer connection. Unactivated
reservations expire after 60 seconds; active membership never expires solely because a control channel becomes
unavailable.

Control keeps its state as system tables (`chunk_hosts`, `chunk_sessions`, `chunk_players`, `chunk_claims`,
`chunk_moves`, `chunk_drains`, `chunk_rosters` and the `chunk_control` row) in a `chunk-store` database under
`.chunk/control/`, with its exclusive writer lock. Each update is one commit with its own operation ID and revision; an
in-memory copy serves reads and is rebuilt from the tables on open. Gameplay data still belongs to the environment
backend. The tables retain requests, reservations and activation intent before external effects. A lost activation reply
is reconciled against the runtime's inventory. Configuration packets travel over the native Minecraft connection;
control carries destination metadata. A player row exists only while it owns a claim. Released claims, and moves that
only reference them, are forgotten five minutes after release. `Control::changes_after` lists claim and move changes
after a log position, and `Control::subscribe` announces new positions.

`Control::move_roster` moves a group to one destination session: it reserves every slot and queues every member's move
in one commit, or changes nothing. Members are admitted together once all of them have asked to activate. Before that,
any member's claim ending, or `Control::cancel_roster`, fails every member's move. Session capacity is the only hard
limit. Until the group is complete, activation fails with `UNAVAILABLE` "roster awaiting members", which gateways retry
within their connection timeout.

`Nodes` reports starting, online, unhealthy, unreachable, draining, stopping and confirmed stopped states, including the
last observed JVM health metrics and observation timestamp. Health is polled every five seconds; missing or stalled
engine progress blocks new placement and three consecutive failures request termination. `ShutdownNode` binds an
operation ID to a node and deadline, retires its capacity, queues player moves, and stops the JVM when empty or at the
deadline. Zero seconds requests immediate termination. `chunk nodes --control-file PATH list` emits JSON;
`shutdown HOST --operation ID --timeout-seconds 60` queues an idempotent shutdown. A queued request is not an exit
acknowledgment.

Graceful control shutdown stops owned JVMs. After an abrupt control-process failure, local child handles cannot be
recovered: durable launch markers retain unresolved ownership and prevent duplicate launches, and such nodes report
unreachable until their JVM re-attaches. Before spawning a JVM, the host writes its process identity and the SHA-256
digest of its credential into the launch marker. A JVM keeps repeating its registration; control accepts it only when
the host's marker matches that credential and identity and the JVM still runs the host's app. Nothing adopts a process
by PID. A re-attached JVM has no child handle, so stopping it sends `StopProcess` but its exit stays unconfirmed; stop
such processes before discarding local state. Hosted providers will need durable provider identities to confirm
termination across control restarts.

After control opens, and again whenever a JVM re-attaches, new claims fail as busy until every surviving JVM is fenced:
its deliveries whose generations no open claim in the log matches are withdrawn with the generation the JVM holds, and
operations the log does not know, such as those a restore lost, become released tombstones that reject retries. A
JVM that has not re-attached and been fenced within 30 seconds stops blocking admission; placement still avoids it.
JVM failure loses transient
worlds; no packets or worlds are replayed. State from the previous shared-classpath runtime is incompatible with this
release.

Local bounds: 32 processes at most, 16 sessions per process at most, 128 declared slots per process and 256 retained
sessions. Claims and moves are bounded only by the store's capacity. At most 1024 claim, activation and cancellation
operations are in flight; beyond that, new work fails as busy (`UNAVAILABLE`, "control busy") and should be retried.
These conservative limits are admission bounds, not a measured memory/tick packing policy. Cross-proxy transfers, hosted
providers, deployment rollout and directory replication remain outside this local implementation.

Focused tests: `cargo test -p chunk-control`.

## Captured session methods

`capture_session` accepts an exact arrived `ClaimIdentity`. `prepare_session_method` checks control's pinned optional
`session_methods` contract and freezes the target, arguments, deadline and a new operation ID. `call_session_method`
sends or polls that prepared operation through the authenticated process endpoint. These are trusted Rust APIs; an
authored TypeScript method reference supplies no player authority.

The operation sequence is allocated durably by control. Retries must reuse the same `PreparedSessionMethod`; preparing
again creates a new operation. The JVM caches exact requests/results for up to five minutes, subject to 4096 records and
a 16 MiB aggregate budget. A monotonic retirement floor prevents evicted operation IDs from running again; late
out-of-order operations below the floor conservatively return unknown. Process identity and generation fence restarts.

Arguments and results each have a 48 KiB UTF-8 JSON limit. The existing wire rules bound nesting to 32 levels, require
finite numbers and limit integral values to ±9,007,199,254,740,991. Deadlines range from 1 ms to 30 seconds. At most 128
method tasks may occupy the tick queue, including canceled work waiting for that queue to drain. Calls recheck exact
session and player membership before executing; departure, finish or cancellation prevents queued gameplay from
starting.

| Result    | Meaning                                                                                       |
| --------- | --------------------------------------------------------------------------------------------- |
| Accepted  | Queued; gameplay may not have started.                                                        |
| Completed | The validated result is available.                                                            |
| Cancelled | Gameplay definitively did not start.                                                          |
| Failed    | Gameplay threw or returned an invalid result; it may have changed state.                      |
| Unknown   | Execution or its result cannot be confirmed. Retry the same prepared operation to learn more. |

A synchronous method already running on the tick thread cannot be forcibly interrupted safely. Cancellation or a missed
deadline then returns unknown, and a later poll may retrieve the completed result. Neither failure nor unknown implies
rollback. These calls are transient gameplay effects. A durable job integration will need a persisted invocation record
and recovery for outcomes that remain unknown; the current prepared-operation API is in memory.

The authenticated `LocalControl` proxy credential can prepare a captured method and receive an opaque handle before any
gameplay executes. `StartPreparedMethod` starts that retained handle once; `PollPreparedMethod` observes it and
`CancelPreparedMethod` cancels its exact token. JVM registration and application/backend credentials cannot use these
RPCs. The authored app, unqualified session and method must match the captured live target and pinned declaration.
`chunk dev` projects these declarations from the published, content-addressed backend contract into control config.

Prepared handles are local to one control service lifetime. Losing a prepare reply is safe because preparation never
executes; an unknown, expired or evicted handle cannot be recreated by starting or polling it. Up to 128 pending
handles, 4096 completed records and 16 MiB of serialized requests/schemas/results are retained. Admission reserves space
for each pending result. Results expire after five minutes or earlier under capacity pressure. Service shutdown cancels
method tokens before awaiting tracked tasks. Accepted means the control task was queued; it does not promise gameplay
has started. Unstarted cancellation is definitive; started cancellation reports unknown until its outcome is resolved.

Proxy-initiated moves also provide the expected source claim and public connection ID together. Control checks both
against the exact current arrived owner in the same durable update that accepts or returns the move. Trusted
administrative moves may omit both fields. Replacing a public connection prevents old captured effects from moving its
new owner.
