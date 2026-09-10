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

## Lifecycle

Backend, control and edge run as tasks in one development process. The backend
retains its dedicated JavaScript and storage threads. Runtime supervisors are
embedded; each gameplay JVM is still a child process.

Ctrl-C closes player connections, drains control operations, stops owned JVMs,
and joins backend workers. An unexpected service exit stops the local stack;
there is no independent process restart in this mode. Use the standalone service
binaries when testing process failure and recovery. An abrupt dev-process crash
can leave an unresolved JVM launch; the next run refuses to claim ownership from
a stale PID. Resolve the leftover JVM before reusing that state.

Service logs go to the console and JVM logs stay in the runtime state directory.
An unresolved shutdown is reported as an error.

## Files and configuration

`project.json` selects the environment, built gameplay distribution, session types
and machine profiles. `gameplay_module` must match the module recorded by Gradle
in the generated distribution; mismatches fail before launching services. Paths are relative to that file. `server/schema/index.ts`
composes the physical schema; `server/*.ts` exports validated function descriptors.
`examples/local/gradlew generateChunkBackend` emits the backend under
`.chunk/build/backend` and shared JVM bindings under `.chunk/generated/jvm`.
The standalone example uses the public Chunk settings and project plugins with
repository composite builds for local framework dependencies. Its temporary
`:gameplay` project keeps the shared session implementation in `jvm/example`.
The discovered `apps/lobby` and `apps/arena` projects each package one
`SessionProvider` service that creates fresh session state. The plugin generates
`META-INF/chunk/app.json` in each app JAR, and the runtime verifies that its
provider belongs to that same JAR. `installDist` includes both app JARs and the
matching backend for the current local runner.

Shared descriptors use `shared/<file>/<export>`; app-local descriptors use
`apps/<app>/<file>/<export>`. The initial managed caller's `app` identifies its
registered app ID. Multiple session instances may belong to the same app.
Annotation-driven registration remains deferred.

Artifacts under `.chunk/local/artifacts/<digest>` include copied JARs, the backend
bundle and project metadata. Content changes produce a new deployment; existing
artifacts are verified before reuse. Stop and rerun after editing the example.
Backend data stays under `.chunk/local/backend`; placement state is separate for
each deployment. This runner does not implement overlapping deployment rollouts.

Connection records under `.chunk/local` contain
private credentials and must not be shared. Backend/control use loopback ports
25568/25567; the public listener uses 25565. The runner refuses occupied ports or a
second owner of its state directory. `just local --state <directory> --bind <address>
--backend-bind <address> --control-bind <address>` selects another local environment;
all addresses must remain loopback.

## Automated backend boundary check

`examples/local/gradlew :gameplay:test --tests '*BackendIntegrationTest'` builds the Rust
backend and generated Java client, then calls the actual TypeScript coin/stat
handlers over authenticated loopback gRPC. It verifies durable operation recovery,
stale/fresh watch transitions across backend restart, shared data between retained
deployments and independent player identities. Its processes, channels and executors
are closed on completion. This complements the official-client scenario above;
it does not simulate Minecraft login or player movement.
