# Local example

From the repository root, install the pinned tools with `mise install`, then:

```sh
just local
```

This installs pinned JS dependencies, compiles TypeScript declarations and handlers,
generates Java records/references before Kotlin compilation, resolves Java 25, packages
an immutable deployment, and starts the backend, control and proxy. Join
`localhost:25565` with a signed-in official Minecraft Java Edition 26.1 client.
Status runs JavaScript without starting gameplay. Login runs admission/routing and
automatically creates a lobby session.

The grass lobby and sandstone arenas share persistent coins and visit counts.
Use `/coin` to commit a mutation; chat and the action bar reflect subscriptions.
A join explicitly reads saved coins and increments visits. Nothing saves world
simulation state across JVM shutdown.

## Moves and drain

The player's UUID appears beside `player=` in `.chunk/local/edge.log`. In another
terminal, substitute that UUID below:

```sh
just players --player <uuid> move --session-type arena --key arena
just players --player <uuid> move --session-type arena --key arena-2
just players --player <uuid> drain --timeout-seconds 60
```

The example allows two sessions per JVM: the first arena shares the lobby's JVM;
the second arena needs another. Moves preserve the public connection. Drain stops
new reservations on the selected runtime, moves its players, and shuts it down
when empty or at the deadline. Operator commands print an operation ID; supply
`--operation <id>` when retrying an uncertain command.

## Backend restart and recovery

While the runner is active, restart only its backend:

```sh
kill -TERM "$(cat .chunk/local/backend.pid)"
```

Gameplay continues. Subscriptions report reconnecting and then resume with a fresh
snapshot. The runner starts the same deployment with the same SQLite database,
endpoint and credential after a short delay. Coins and visits survive; disconnect
and rejoin to verify a fresh login reads them.

An abruptly crashed control process is also restarted against its existing durable
authority, preserving surviving runtime connections. Repeated service failures are
bounded and reported. An edge crash loses public sockets; a runtime/JVM crash loses
that gameplay delivery. The proxy sends a bounded disconnect and a subsequent login
can provision replacement capacity. World state and player TCP packets are not replayed.

Ctrl-C stops the proxy, control, runtimes/JVMs and backend. The runner also attempts
runtime cleanup from private connection records if control has already failed.
An unresolved shutdown is reported as an error. Tailscale and other applications
are unaffected.

## Files and configuration

`project.json` selects the environment, built gameplay distribution, session types
and machine profiles. `gameplay_module` must match the module recorded by Gradle
in the generated distribution; mismatches fail before launching services. Paths are relative to that file. `server/schema/index.ts`
composes the physical schema; `server/*.ts` exports validated function descriptors.
`jvm:example:generateBackend` emits the bundle, contract, source map and Java/TS
clients under the module's build directory. The `chunk.backend-generation`
convention plugin wires generation before Java/Kotlin compilation and includes
the matching backend in `installDist`. The runner publishes that distribution's
backend and JARs together; no contract JSON or gRPC configuration is handwritten.

Apply that convention to another JVM application and configure its
`GenerateBackend` task with `backendProject` and `packageName`. Shared descriptors
use `shared/<file>/<export>`; app-local descriptors use `apps/<app>/<file>/<export>`.
The initial managed caller's `app` identifies its registered session type.
Standalone app manifests and annotation-driven module discovery remain deferred.

Artifacts under `.chunk/local/artifacts/<digest>` include copied JARs, the backend
bundle and project metadata. Content changes produce a new deployment; existing
artifacts are verified before reuse. Stop and rerun after editing the example.
The platform executable is also pinned under `.chunk/local/platform`, so rebuilding
the CLI cannot change or invalidate child launches during a running session.
Backend data stays under `.chunk/local/backend`; placement state is separate for
each deployment. This runner does not implement overlapping deployment rollouts.

Logs and live service PID files are under `.chunk/local`. Connection records contain
private credentials and must not be shared. Backend/control use loopback ports
25568/25567; the public listener uses 25565. The runner refuses occupied ports or a
second owner of its state directory. `just local --state <directory> --bind <address>
--backend-bind <address> --control-bind <address>` selects another local environment;
all addresses must remain loopback.

## Automated backend boundary check

`./gradlew :jvm:example:test --tests '*BackendIntegrationTest'` builds the Rust
backend and generated Java client, then calls the actual TypeScript coin/stat
handlers over authenticated loopback gRPC. It verifies durable operation recovery,
stale/fresh watch transitions across backend restart, shared data between retained
deployments and independent player identities. Its processes, channels and executors
are closed on completion. This complements the official-client scenario above;
it does not simulate Minecraft login or player movement.
