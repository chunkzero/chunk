# Gameplay JVM

Minestom accepts prepared players through its normal loopback Minecraft listener. The proxy owns public authentication,
encryption and compression and relays Minestom configuration and play. The Rust runtime supervises the JVM and relays
each player's connection. Minestom uses protocol 775, compatible with Java Edition 26.1.

`jvm:runtime` is a Java platform with no Kotlin standard library or coroutine production dependency. It runs on JDK 25
and starts through `dev.chunkzero.runtime.BridgeMain`. Kotlin lifecycle conveniences are provided by
`jvm:runtime-kotlin`.

Use `just local` to run the complete example, or build `cargo build -p chunk-runtime` and
`./gradlew :jvm:runtime:installDist` for independent hosting. `chunk-runtime` reads configuration exclusively from the
environment:

- `CHUNK_DISTRIBUTION`, `CHUNK_JAVA`, `CHUNK_CONNECTION`
- `CHUNK_ENVIRONMENT`, `CHUNK_DEPLOYMENT`, `CHUNK_MACHINE_PROFILE`
- `CHUNK_ARTIFACT_DIGEST`, `CHUNK_MEMORY_MIB`
- Optional `CHUNK_BACKEND_FILE`

The runtime starts empty; control provisions gameplay sessions. It generates separate child and proxy-facing credentials
and publishes its private connection record only after authenticated registration and advancing ticks. JVM logs use the
connection filename with a `.log` extension. Shutdown awaits JVM exit before writing an `.exit` acknowledgment. Rust
hosts can call `server::Config::launch` and explicitly await `ManagedJvm::stop` for the same lifecycle in-process.

Registration freezes deployment, runtime/process incarnation, machine profile, artifact identity, protocol version and
both JVM endpoints. Inventory RPCs report ticks and prepared/attached/closed delivery bindings. A lifecycle outage marks
inventory unavailable and rejects new preparation, while existing TCP streams remain independent. Repeated registration
reconciles the same process; it cannot change endpoints or configuration. A dead runtime loses its relays; a dead JVM
loses its worlds. Neither is recovered by replaying player bytes.

Preparation creates no player. Single-use capabilities expire after thirty seconds and are exchanged through standard
`chunk:delivery` login plugin packets. The runtime replaces its upstream capability with the JVM capability on the
second hop, then forwards normal login success and relays bounded byte buffers. Minestom owns configuration and player
creation. Slow relay writes expire after five seconds; login is bounded to five seconds, sockets to 128 and history to
4096 operations. Native Minestom sockets handle buffering and graceful kicks.

Focused checks: `cargo test -p chunk-runtime` and `./gradlew :jvm:runtime:test`. The standalone bridge remains available
with `CHUNK_PROCESS_TOKEN` (at least 32 characters), `CHUNK_ENVIRONMENT` and `CHUNK_DEPLOYMENT`; without
`CHUNK_SUPERVISOR` it binds control on 25566 and uses a fixed fixture incarnation. Production local launches should use
the supervisor.

Each discovered app packages a public `SessionProvider` with a no-argument constructor and a `Session create()` method.
The app owns exactly one service entry in `META-INF/services/dev.chunkzero.runtime.SessionProvider`; the Gradle plugin
generates `META-INF/chunk/app.json` with its directory-derived app ID. The runtime requires metadata and the provider
class to come from that same app JAR. Registration uses packaged JARs on one shared classpath. The factory creates fresh
session state, and multiple sessions can use the same app.

Session implementations extend `Session`. `onCreate`, `onJoin`, `onLeave` and `onFinish` return `CompletionStage<Void>`
and begin on the process tick thread. Do not block that thread. Resume asynchronous world changes with
`scope.onTick(() -> ...)` in Java or `scope.onTick { ... }` in Kotlin. Creation becomes ready only after its stage
completes and at least one instance exists. `scope.finish()` requests ending; do not await it from a lifecycle hook
whose own completion ending must await.

`SessionScope` owns up to 16 instances, player-filtered events, repeating tasks and registered `AutoCloseable` resources
such as subscriptions. Instance event nodes remain available for instance-local events. Direct global registrations
require explicit cleanup. Ending withdraws deliveries, waits for leave/finish hooks, then removes only that scope's
listeners, tasks, resources and instances. A process retains at most 256 session identities and 4096 delivery
operations; exhausting history requires a replacement process. Stuck customer futures retain ownership until a
supervisor deadline terminates the process; they never produce a false withdrawal acknowledgment.

Delivery pins session generation as well as process/deployment and player ownership. A prepared delivery reserves
capacity. Native Minecraft login attaches the player. `onJoin` begins after Minestom spawn completes, so it can send
player UI and start asynchronous backend work. Arrival requires the join stage and the latest teleport acknowledgment.
Withdrawal fences output immediately, waits for pending Minestom spawn callbacks and the join stage, removes the old
player and completes its leave hook before releasing UUID ownership. The proxy must acknowledge withdrawal before
activating that UUID elsewhere in the JVM. Managed proxy moves prepare a new TCP delivery while the source plays,
confirm withdrawal, drive both client configuration acknowledgments, and activate the destination on the existing public
connection.

The optional `scope.getBackend()` client (`scope.backend` in Kotlin) is bound to the process deployment, session and
registered app ID. Function arguments do not choose that identity. Control's `CHUNK_BACKEND_FILE` passes the private
connection to supervised JVMs. Use `scope.operationId(player, action)` for a mutation that should happen once per player
delivery. It returns a typed `OperationId`; retry an uncertain result with the same ID and arguments. Use
`scope.coroutines.backend(scope.backend, player)` for a player-bound client whose calls and watches close on departure.
Session clients close on disposal. The [local example](../../examples/local/README.md) demonstrates persistent coins,
visits and subscription updates, including stale state during backend disconnection. Sessions own their instances, event
handlers and scoped resources. Session hooks run through the process tick executor. Withdrawal waits for pending joins
and initialization, removes the player and runs its leave hook before releasing the ownership fence. Arrival is reported
after spawn and teleport acknowledgment.

Kotlin applications depend on `jvm:runtime-kotlin`, import `dev.chunkzero.runtime.coroutines`, and can extend
`CoroutineSession` and implement suspend `create`, `join`, `leave`, and `finish` hooks. `scope.coroutines` owns their
jobs and resumes continuations on the process tick thread. Wrap a Java `BackendSession` with
`scope.coroutines.backend(client)` for suspend calls and bounded `Flow` watches; the overload accepting an admitted
`Player` creates a child identity and closes its calls/watches on departure. Player resources use object identity so
cleanup cannot affect a later admission of the same UUID.

Put final result mutations in `finish()`: the manager awaits that hook before closing session resources. Request
termination with `scope.finish()` without awaiting it from work that the same termination will cancel. Slow Flow
collectors fail at 64 queued updates instead of dropping stale transitions. Both languages use the same
`SessionProvider.create()` registration contract. The [Java consumer](../../examples/java/README.md) demonstrates the
Java lifecycle and generated client without Kotlin dependencies. The coroutine adapters live in `jvm:runtime-kotlin`;
backend-only Java consumers use `jvm:backend-client`.
