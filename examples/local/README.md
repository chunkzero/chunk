# Local example

From the repository root, install the pinned tools with `mise install`, then:

```sh
just local
```

This installs pinned JS dependencies, compiles TypeScript declarations and handlers, generates shared JVM clients before
Kotlin compilation, resolves Java 25, packages an immutable deployment, and starts the backend, control and proxy. Join
`localhost:25565` with a signed-in official Minecraft Java Edition 26.2 client. Status runs JavaScript without starting
gameplay. Login runs admission/routing and automatically creates a lobby session.

To build the complete release without starting services:

```sh
just toolchain
target/debug/chunk build examples/local
```

The project Gradle wrapper builds both apps and supplies its Java toolchain in
`examples/local/.chunk/build/jvm/artifacts.json`. The CLI packages those outputs under `examples/local/dist/<id>/` and
`examples/local/dist/<id>.tar.gz`.

The grass lobby and sandstone arenas share persistent coins and visit counts. Use `/coin` to commit a mutation; chat and
the action bar reflect subscriptions. A join explicitly reads saved coins and increments visits. Nothing saves world
simulation state across JVM shutdown.

The backend owns `/hello <message>` and `/travel lobby|arena|large` in every scope. Travel uses the same admission and
capacity policy as operator moves. In the lobby, `/population` calls the generated JVM method on the session captured
when the command starts. After moving to an arena, that command disappears from the client tree. `/coin` remains a JVM
command. Tab completion suggests the declared destinations; `/hello` sends plain-text message and title effects.

These commands are exercised by automated protocol and dispatch checks. Display and movement with an official client
still need the manual scenario above.

For editor setup without starting services, run:

```sh
cargo run -p chunk-cli -- codegen examples/local
```

This materializes the ignored SDK under `examples/local/.chunk/`. Application modules import builders and named types
from `#chunk`; schema modules use `#chunk/schema`. Both resolve through `examples/local/package.json`.

## Moves and drain

The player's UUID appears beside `player=` in the service console logs. In another terminal, substitute that UUID below:

```sh
just players --player <uuid> move --session-type arena/default --key arena
just players --player <uuid> move --session-type arena/default --key arena-large --machine-profile large
just players --player <uuid> drain --timeout-seconds 60
```

The example allows two sessions per JVM. Lobby and arena use separate JVMs; two default arenas can share one arena JVM.
The `arena-large` destination uses the same `arena/default` implementation with a different typed configuration and
32-player capacity on a 1024 MiB JVM (`--machine-profile large`). Moves preserve the public connection. Drain stops new
reservations on the selected runtime, moves its players, and shuts it down when empty or at the deadline. Operator
commands print an operation ID; supply `--operation <id>` when retrying an uncertain command.

## Lifecycle

Backend, control and proxy run as tasks in one development process. The backend retains its dedicated JavaScript and
storage threads. Control owns each gameplay JVM directly as a child process.

Ctrl-C closes player connections, drains control operations, stops owned JVMs, and joins backend workers. An unexpected
service exit stops the local stack; there is no independent process restart in this mode. Run the `chunk-environment`
binary when testing process failure and recovery. An abrupt dev-process crash can leave an unresolved JVM launch; the
next run refuses to claim ownership from a stale PID. Resolve the leftover JVM before reusing that state.

Service logs go to the console and JVM logs stay in the runtime state directory. An unresolved shutdown is reported as
an error.

## Files and configuration

`chunk.toml` selects the local environment and default runtime requirements: 16 players per session, two sessions per
512 MiB JVM, and at most four JVMs. `apps/lobby/app.ts` and `apps/games/arena/app.ts` declare stable app IDs, runtime
requirements and destinations. Their implementations are addressed as `lobby/default` and `arena/default`; moving an app
directory does not change its ID. `apps/scope.ts` supplies inherited hooks/commands and initial routing. Java 25 remains
explicit in the Gradle builds. The local runner uses Gradle's selected executable, with an optional `--java PATH`
override.

`server/schema/index.ts` composes the physical schema; `server/*.ts` exports validated function descriptors. Paths in
this paragraph are relative to `examples/local`. `examples/local/gradlew generateChunkBackend` emits the backend under
`examples/local/.chunk/build/backend` and shared JVM bindings under `examples/local/.chunk/generated/jvm`. The
standalone example uses the public Chunk settings and project plugins with repository composite builds for local
framework dependencies. Its explicit `shared` project contains the common session implementation and backend boundary
test. The discovered `:apps:lobby` and `:apps:games:arena` projects package annotated providers that create fresh
session state. The plugin generates a local Java service registry from those annotations. The arena implements the
generated `ArenaSessionProviders.Default` interface and receives a typed `SessionCreation` containing its fixed
configuration and `maxPlayers`. Standard and large destinations reuse this provider; they do not need separate classes.
Destination references come from `#chunk/apps` without importing executable app modules. Gradle retains
dependency/toolchain settings. `chunkArtifacts` builds independent executable app JARs containing the generated backend
client and runtime libraries. Each app supplies `application.mainClass`; its main connects to Chunk, starts Minestom,
explicitly calls `ready()` and waits for shutdown.

Shared descriptors use `shared/<file>/<export>`; app-local descriptors use `apps/<app>/<file>/<export>`. The initial
managed caller's `app` identifies its registered app ID. Multiple session instances may belong to the same app. Session
factories use `@SessionType("default")`; there are no handwritten service registration resources.

Releases under `examples/local/dist/<id>` include JARs, the backend bundle, normalized release metadata and explicitly
supplied assets. Each release also has a sibling `.tar.gz` archive. Content changes produce a new deployment; existing
releases are verified before reuse. Stop and rerun after editing the example. Backend data stays under
`examples/local/.chunk/local/backend`; placement state is separate for each deployment. This runner does not implement
overlapping deployment rollouts.

Connection records under `examples/local/.chunk/local` contain private credentials and must not be shared.
Backend/control use loopback ports 25568/25567; the public listener uses 25565. The runner refuses occupied ports or a
second owner of its state directory.
`just local --state <directory> --bind <address> --backend-bind <address> --control-bind <address>` selects another
local environment; all addresses must remain loopback.

Inspect nodes with `chunk nodes --control-file examples/local/.chunk/local/control.json list`. Request a node shutdown
with the same prefix followed by `shutdown HOST --operation UUID --timeout-seconds 60`; retain the operation ID for
retries. The response means shutdown was queued. Poll `list` for a confirmed `NODE_PHASE_STOPPED` phase. This rework
requires rebuilding releases; old local control state is incompatible.
