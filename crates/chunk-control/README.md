# Local control

`chunk-control` is an environment-configured service; `chunk_control::server::run`
is its embeddable entry point. Build with `cargo build -p chunk-control -p chunk-runtime`.
Required environment variables:

- `CHUNK_STATE`, `CHUNK_CONNECTION`: durable state directory and discovery record.
- `CHUNK_CONFIG`: JSON `chunk_control::Config` with deployment, profiles and session types.
- `CHUNK_RUNTIME_EXECUTABLE`: path to the standalone `chunk-runtime` executable.
- `CHUNK_DISTRIBUTION`, `CHUNK_JAVA`: gameplay distribution and Java 25 executable.
- Optional `CHUNK_BACKEND_FILE` and `CHUNK_BIND` (default `127.0.0.1:25567`).

`chunk local` constructs the same typed configuration and uses `EmbeddedHost` to
own Rust supervisors in-process. Standalone control uses `ProcessHost` to launch
runtime executables.

The private `.chunk/control.json` connection file authorizes gRPC calls. `Claim`
accepts authenticated identity, proxy incarnation, connection identity and a session
demand key/type/profile. It reserves capacity, starts a separate runtime process if
needed, waits for session readiness, and returns configuration plus a single-use
TCP capability. The proxy records admission intent with `Activate` and opens the native Minecraft
connection using the capability. `Inspect` reconciles the same binding; `Cancel` withdraws
it before releasing the reservation. Runtime credentials remain inside control.

Concurrent demand shares compatible sessions up to declared capacity. A prepared
slot is a reservation, not a second attached player. Each membership and delivery
has a separate monotonic generation; the session and process have their own
incarnations. Duplicate login is rejected while an earlier owner remains unresolved.
An old cancellation cannot release a newer connection. Unactivated reservations
expire after 60 seconds; active membership never expires solely because a control
channel becomes unavailable.

Control uses a separate SQLite database and exclusive writer lock under
`.chunk/control/`. Gameplay data still belongs to the environment backend. The
control database retains requests, generation counters, reservations and activation
intent before external effects. A lost activation reply is reconciled against the
runtime's inventory. Configuration packets travel over the native Minecraft
connection; control carries destination metadata.

Runtime processes outlive abrupt control-process failure. Restart with the same
state directory and configuration to reconcile surviving player streams. Durable
launch markers prevent duplicate hosts when launch outcome is uncertain. Missing
connection files are unresolved, not proof that a process stopped; the runtime
writes a separate exit marker only after confirmed JVM cleanup. Persisted PIDs
are diagnostic data; recovery never signals an unverified PID. If authenticated
runtime shutdown is unavailable, termination reports an unresolved outcome. Graceful control
shutdown stops its runtime processes. Process/JVM failure loses transient worlds;
no packets or worlds are replayed.

Local bounds: 32 processes at most, 16 sessions per process at most, 128 declared
slots per process, 256 retained sessions and 1024 retained claims. The state document
also inherits the storage byte limit. These conservative limits are admission
bounds, not a measured memory/tick packing policy. Cross-proxy transfers, hosted
providers, deployment rollout and directory replication remain outside this local
implementation.

Focused tests: `cargo test -p chunk-control`.
