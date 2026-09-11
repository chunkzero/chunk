# Local control

`chunk-control` is an environment-configured service; `chunk_control::server::run` is its embeddable entry point. Build
with `cargo build -p chunk-control`. Required environment variables:

- `CHUNK_STATE`, `CHUNK_CONNECTION`: durable state directory and discovery record.
- `CHUNK_CONFIG`: JSON `chunk_control::Config` with deployment, executable app manifests, profiles and session types.
- `CHUNK_DISTRIBUTION`, `CHUNK_JAVA`: release directory and Java 25 executable.
- `CHUNK_BACKEND_FILE`: backend connection bound to the same deployment.
- Optional `CHUNK_BIND` (default `127.0.0.1:25567`).

`chunk dev` and standalone control use `ProcessHost` to launch each app with `java -jar`. It owns the child handle and
waits for exit; there is no per-server sidecar. Future container/machine providers implement the same `Host` boundary.

The private `.chunk/control.json` connection file authorizes gRPC calls. `Claim` accepts authenticated identity, proxy
incarnation, connection identity and a session demand key/type/profile. It reserves capacity, starts an app JVM if
needed, waits for session readiness, and returns configuration plus a single-use TCP capability. The proxy records
admission intent with `Activate` and opens the native Minecraft connection using the capability. `Inspect` reconciles
the same binding; `Cancel` withdraws it before releasing the reservation. Runtime credentials remain inside control.

Concurrent demand shares compatible sessions up to declared capacity. JVM placement matches both app and machine
profile. A prepared slot is a reservation, not a second attached player. Each membership and delivery has a separate
monotonic generation; the session and process have their own incarnations. Duplicate login is rejected while an earlier
owner remains unresolved. An old cancellation cannot release a newer connection. Unactivated reservations expire after
60 seconds; active membership never expires solely because a control channel becomes unavailable.

Control uses a separate SQLite database and exclusive writer lock under `.chunk/control/`. Gameplay data still belongs
to the environment backend. The control database retains requests, generation counters, reservations and activation
intent before external effects. A lost activation reply is reconciled against the runtime's inventory. Configuration
packets travel over the native Minecraft connection; control carries destination metadata.

`Nodes` reports starting, online, unhealthy, unreachable, draining, stopping and confirmed stopped states, including the
last observed JVM health metrics and observation timestamp. Health is polled every five seconds; missing or stalled
engine progress blocks new placement and three consecutive failures request termination. `ShutdownNode` binds an
operation ID to a node and deadline, retires its capacity, queues player moves, and stops the JVM when empty or at the
deadline. Zero seconds requests immediate termination. `chunk nodes --control-file PATH list` emits JSON;
`shutdown HOST --operation ID --timeout-seconds 60` queues an idempotent shutdown. A queued request is not an exit
acknowledgment.

Graceful control shutdown stops owned JVMs. After an abrupt control-process failure, local child handles cannot be
recovered safely: durable launch markers retain unresolved ownership and prevent duplicate launches. Such nodes report
unreachable. Persisted numeric PIDs never authorize a kill. This local host does not yet recover or automatically clean
up orphaned JVMs after a hard control crash; stop those processes before discarding their local state. Hosted providers
will need durable provider identities to confirm termination across control restarts. JVM failure loses transient
worlds; no packets or worlds are replayed. State from the previous shared-classpath runtime is incompatible with this
release.

Local bounds: 32 processes at most, 16 sessions per process at most, 128 declared slots per process, 256 retained
sessions and 1024 retained claims. The state document also inherits the storage byte limit. These conservative limits
are admission bounds, not a measured memory/tick packing policy. Cross-proxy transfers, hosted providers, deployment
rollout and directory replication remain outside this local implementation.

Focused tests: `cargo test -p chunk-control`.
