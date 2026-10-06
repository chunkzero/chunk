# JVM runtime

The library that connects a gameplay JVM to its environment's core. `ChunkProcess` reads the launch configuration,
registers the JVM once the app is ready, follows core's instructions for sessions, player deliveries and session
methods, reports health, and turns core's stop request into a shutdown signal. It is engine-independent Java 25 with no
Minestom or Kotlin dependency.

Apps use it together with an engine adapter:

| Module                                    | Artifact                               | Contents                                                                |
| ----------------------------------------- | -------------------------------------- | ----------------------------------------------------------------------- |
| `jvm/runtime`                             | `com.chunkzero.chunk:runtime`          | `ChunkProcess`, `@SessionType`, `@Component`                            |
| [`jvm/multistom`](../multistom/README.md) | `com.chunkzero.chunk:multistom`        | The session API and its runtime on the multistom Minestom fork          |
| `jvm/multistom-kotlin`                    | `com.chunkzero.chunk:multistom-kotlin` | Coroutine adapters, covered in the multistom README                     |
| [`jvm/minestom`](../minestom/README.md)   | `com.chunkzero.chunk:minestom`         | Login handling for apps running their own sessions on upstream Minestom |

The [Gradle plugin](../gradle-plugin/README.md) adds the right one to each app.

## Process lifecycle

Every app is an executable JAR whose `main` connects, starts the engine, reports ready and waits:

```java
public static void main(String[] args) throws Exception {
    try (var chunk = ChunkProcess.connect();
            var minestom = ChunkMinestom.attach(chunk, ServerProcess.create())) {
        // Process-wide setup, such as Minestom commands, goes here.
        minestom.start();
        chunk.ready();
        chunk.awaitShutdown();
    }
}
```

- `ChunkProcess.connect()` reads the launch configuration below and fails if any of it is missing. An app JAR therefore
  runs only under `chunk dev` or an environment, not with a bare `java -jar`.
- `ready()` requires a started engine. It registers the JVM with core, retrying while core is unreachable, and throws if
  core refuses the credential or the deployment is not active. Until then, no sessions are placed on the JVM.
- `awaitShutdown()` blocks until shutdown is requested, by core's stop instruction or `requestShutdown()`;
  `shutdownRequested()` is the same as a `CompletionStage`. If core rejects the JVM's credential for good,
  `awaitShutdown()` throws, so the process exits non-zero.
- Closing `ChunkMinestom` stops Minestom and disposes every session; closing `ChunkProcess` ends the link to core. The
  host gives the process a bounded time to exit before killing it.

After `ready()`, the JVM follows its `jvm/<host>` topic on core's `chunk.sync.v1` `Core` service for the sessions to
create, the players to admit and the session methods to run, and reports what it holds and its health (engine tick
progress, heap, GC, CPU, sessions and players) at least every three seconds. A broken stream registers and subscribes
again. Players do not pass through this link: the gateway connects to Minestom's own listener, and each login presents a
single-use capability the JVM issued for that player's delivery.

## Running your own sessions

The `multistom` module decides what a session is: a `Session` with its own scope in a shared Minestom process. An engine
adapter, or an app that wants other isolation, can instead implement `SessionHandler` and pass it to `chunk.host(...)`.
The returned `ChunkSessions` keeps the accounting core relies on, whatever a session is:

| Chunk keeps                                                          | The handler decides                                       |
| -------------------------------------------------------------------- | --------------------------------------------------------- |
| Session phases, the 10-second creation deadline, 256 live sessions   | What a session is and how it is isolated from the others  |
| Delivery capabilities, identity checks and fencing, session capacity | Where an admitted player spawns, and when they've arrived |
| Withdrawing a session's players before its handler finishes it       | How it tears a session down                               |
| Which session methods may run, and for whom                          | What a session method does                                |

```java
try (var chunk = ChunkProcess.connect()) {
    var sessions = chunk.host(new SessionHandler() {
        @Override public void create(SessionControl session) {
            // session.id(), type(), capacity(), configurationJson(), backend()
            games.open(session).whenComplete((ok, error) -> {
                if (error == null) session.ready();
                else session.fail(error);
            });
        }

        @Override public void finish(SessionControl session) {
            games.close(session.id()).whenComplete((ok, error) -> {
                if (error == null) session.ended();
                else session.ended(error);
            });
        }
    });
    // Start the engine's listener on chunk.playerAddress(), leaving compression and encryption to the gateway.
    chunk.bind(port, protocolVersion);
    chunk.ready();
    chunk.awaitShutdown();
}
```

- Handler callbacks run one at a time on the host's own thread and must return promptly. Report through the
  `SessionControl` from any thread.
- `finish` runs once every player of the session has been released, including after a failed creation. A session counts
  against the JVM until it has `ended()`.
- Each login presents the payload of its `chunk:delivery` login plugin response.
  `sessions.admit(setup, uuid, name, disconnect)` checks it and returns the player's `Delivery`; admit the player with
  its `player()` profile, and call `arrived()` once they are in the session. If core withdraws the delivery,
  `disconnect` runs; once the player's connection closed and any leave handling settled, call `release()`.
- Call `chunk.tick()` once per engine tick, so core can tell a stalled JVM.
- `Delivery.move(destination)` and `operationId(action)` work as `SessionScope`'s do. Session methods reach
  `SessionHandler.method`, which no session declares by default; a handler calls `SessionMethod.start()` right before
  running the method's effects and skips them if it returns false.

[`minestom`](../minestom/README.md) handles the login side for upstream Minestom.

## Launch configuration

The host (control's local process host under `chunk dev`, or `chunk-jvm` on a JVM machine) sets these variables:

| Variable                                       | Meaning                                                                                    |
| ---------------------------------------------- | ------------------------------------------------------------------------------------------ |
| `CHUNK_CORE_ENDPOINT`                          | Core's gRPC endpoint (`CHUNK_CONTROL_ENDPOINT` is read when it is unset)                   |
| `CHUNK_PROCESS_TOKEN`                          | The JVM's credential                                                                       |
| `CHUNK_DEPLOYMENT`                             | The deployment it serves                                                                   |
| `CHUNK_APP_ID`                                 | The app it runs                                                                            |
| `CHUNK_PROCESS_ID`, `CHUNK_PROCESS_GENERATION` | Its identity as one launch of a host                                                       |
| `CHUNK_MACHINE_PROFILE`                        | The machine profile it was launched for                                                    |
| `CHUNK_ARTIFACT_DIGEST`                        | The digest of the app JAR                                                                  |
| `CHUNK_PLAYER_ADDRESS`                         | Optional. The IP Minestom binds for players; loopback or private only, default `127.0.0.1` |
| `CHUNK_ENVIRONMENT_NAME`                       | Optional. The environment's name, whose `[env.<name>.vars]` the generated `Vars` apply     |
