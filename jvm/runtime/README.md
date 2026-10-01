# JVM runtime

The library that connects a gameplay JVM to its environment's core. `ChunkProcess` reads the launch configuration,
registers the JVM once the app is ready, follows core's instructions for sessions, player deliveries and session
methods, reports health, and turns core's stop request into a shutdown signal. It is engine-independent Java 25 with no
Minestom or Kotlin dependency.

Apps use it together with an engine adapter:

| Module                                                  | Artifact                                | Contents                                                    |
| ------------------------------------------------------- | --------------------------------------- | ----------------------------------------------------------- |
| `jvm/runtime`                                           | `dev.chunkzero:runtime`                 | `ChunkProcess`, `@SessionType`, `@Component`                |
| [`jvm/runtime-minestom`](../runtime-minestom/README.md) | `dev.chunkzero:runtime-minestom`        | The Minestom adapter and the session API gameplay code uses |
| `jvm/runtime-minestom-kotlin`                           | `dev.chunkzero:runtime-minestom-kotlin` | Coroutine adapters, covered in the Minestom README          |

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
