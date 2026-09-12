# Integrated server library

`jvm:runtime` is the engine-independent Java 25 library. `ChunkProcess` owns immutable deployment identity, backend
binding, registration, explicit readiness, health reporting and shutdown notification. It has no Minestom or Kotlin
production dependency. `jvm:runtime-minestom` supplies `ChunkMinestom`, sessions, players, worlds, tick scheduling and
Minecraft admission. `jvm:runtime-minestom-kotlin` adds coroutine conveniences to the Minestom integration. Hytale integration is
future work.

Each app is an executable JAR containing its own dependencies. Configure `application.mainClass` in the app's Gradle
build:

```java
@SessionType("default")
public final class Lobby implements SessionProvider {
    public static void main(String[] args) throws Exception {
        try (var chunk = ChunkProcess.connect();
             var minestom = ChunkMinestom.attach(chunk, MinecraftServer.init())) {
            // Initialize application resources here.
            minestom.start();
            chunk.ready();
            chunk.awaitShutdown();
        }
    }

    @Override public Session create() { return new LobbySession(); }
}
```

The build plugin scans compiled `@SessionType` factory classes and generates `META-INF/chunk/app.json`. Factories must
implement `SessionProvider` and have a public no-argument constructor. A factory may override deployment defaults with
`@SessionType(value = "large", machineProfile = "large", capacity = 32)`. Control addresses that session as
`appId/large` and places it only in a JVM assigned to that app and profile. Main functions do not advertise sessions or
choose a deployment. There is exactly one app catalog per executable; JAR and manifest digests are checked during launch
and registration. Changing a factory, profile or main class requires a new release.

The platform supplies `CHUNK_PROCESS_TOKEN`, `CHUNK_ENVIRONMENT`, `CHUNK_DEPLOYMENT`, `CHUNK_CONTROL_ENDPOINT`,
`CHUNK_INSTANCE_ID`, `CHUNK_PROCESS_ID`, `CHUNK_PROCESS_GENERATION`, `CHUNK_MACHINE_PROFILE`, `CHUNK_ARTIFACT_DIGEST`,
`CHUNK_APP_ID`, `CHUNK_BACKEND_ENDPOINT` and `CHUNK_BACKEND_TOKEN`. `ChunkProcess.connect()` verifies the exact backend
deployment before returning. There is no deployment fallback or standalone unbound fixture mode.

`ready()` registers the frozen process only after application initialization. Control polls the authenticated
`NodeControl` health RPC every five seconds. JVM RPC threads report engine progress recorded by the tick thread, heap
use, GC counters, CPU load and session/player counts. Three failed or stale-progress polls request termination; a
reachable RPC thread alone does not establish engine health. Missing measurements retain their observation timestamp.

`shutdownRequested()` provides a completion stage; `awaitShutdown()` is the blocking main-thread equivalent.
`requestShutdown()` lets the app stop accepting work and notify its shutdown handler. The app closes engine resources
when shutdown is requested. The host allows a bounded graceful shutdown, then kills the owned process if necessary and
waits for exit. Player ownership is released only after withdrawal or confirmed process exit.

The proxy connects directly to Minestom's loopback Minecraft listener. Single-use `chunk:delivery` capabilities
authorize native login. There is no per-server Rust process or intermediate TCP relay. Minestom owns configuration,
socket buffering, player creation and worlds. The current host and engine adapter are local; hosted networking/providers
require additional implementations. Use `just local` to run the example.

The `dev.chunkzero.runtime` package retains the public app/session API. Generic process wiring lives in `bootstrap`
and `control`; Minestom wiring lives in `minestom.internal`. Cross-package implementation APIs are marked
`@ApiStatus.Internal`. Health reports active sessions; completed and failed sessions remain in inventory for replay.

Session implementations extend `Session`. `onCreate`, `onJoin`, `onLeave` and `onFinish` return `CompletionStage<Void>`
and begin on the process tick thread. Do not block that thread. Resume asynchronous world changes with
`scope.onTick(() -> ...)` in Java or `scope.onTick { ... }` in Kotlin. Creation becomes ready only after its stage
completes and at least one instance exists. `scope.finish()` requests ending; do not await it from a lifecycle hook
whose own completion ending must await.

`SessionScope` owns up to 16 instances, player-filtered events, repeating tasks and registered `AutoCloseable` resources
such as subscriptions. Instance event nodes remain available for instance-local events. Direct global registrations
require explicit cleanup. Ending withdraws deliveries, waits for leave/finish hooks, then removes only that scope's
listeners, tasks, resources and instances. A process retains at most 256 session identities and 4096 delivery
operations; exhausting history requires a replacement process. Stuck customer futures retain ownership until a host
deadline terminates the process; they never produce a false withdrawal acknowledgment.

Delivery pins session generation as well as process/deployment and player ownership. A prepared delivery reserves
capacity. Native Minecraft login attaches the player. `onJoin` begins after Minestom spawn completes, so it can send
player UI and start asynchronous backend work. Arrival requires the join stage and the latest teleport acknowledgment.
Withdrawal fences output immediately, waits for pending Minestom spawn callbacks and the join stage, removes the old
player and completes its leave hook before releasing UUID ownership. The proxy must acknowledge withdrawal before
activating that UUID elsewhere in the JVM. Managed proxy moves prepare a new TCP delivery while the source plays,
confirm withdrawal, drive both client configuration acknowledgments, and activate the destination on the existing public
connection.

The `scope.getBackend()` client (`scope.backend` in Kotlin) is bound to the process deployment, session and registered
app ID. Function arguments do not choose that identity. Control's `CHUNK_BACKEND_FILE` passes the private connection to
app JVMs as `CHUNK_BACKEND_ENDPOINT` and `CHUNK_BACKEND_TOKEN`. The JVM requires explicit `CHUNK_ENVIRONMENT` and
`CHUNK_DEPLOYMENT` and checks that exact deployment with `CheckDeployment` before reporting ready. Missing
configuration, an unavailable backend, or a missing deployment fails startup. Use `scope.operationId(player, action)`
for a mutation that should happen once per player delivery. It returns a typed `OperationId`; retry an uncertain result
with the same ID and arguments. Use `scope.coroutines.backend(scope.backend, player)` for a player-bound client whose
calls and watches close on departure. Session clients close on disposal. The
[local example](../../examples/local/README.md) demonstrates persistent coins, visits and subscription updates,
including stale state during backend disconnection. Sessions own their instances, event handlers and scoped resources.
Session hooks run through the process tick executor. Withdrawal waits for pending joins and initialization, removes the
player and runs its leave hook before releasing the ownership fence. Arrival is reported after spawn and teleport
acknowledgment.

Kotlin applications depend on `jvm:runtime-minestom-kotlin`, import `dev.chunkzero.runtime.coroutines`, and can extend
`CoroutineSession` and implement suspend `create`, `join`, `leave`, and `finish` hooks. `scope.coroutines` is a
session-owned `CoroutineScope` with the process tick dispatcher as its default. Use standard coroutine extensions such
as `kotlinx.coroutines.launch`, `async`, `future`, and `Flow.launchIn`; disposal cancels the scope's jobs. Wrap a Java
`BackendSession` with `scope.coroutines.backend(client)` for suspend calls and bounded `Flow` watches; the overload
accepting an admitted `Player` creates a child identity and closes its calls/watches on departure. Player resources use
object identity so cleanup cannot affect a later admission of the same UUID.

Import the extensions in `dev.chunkzero.runtime` for `scope.resource<MyResource> { ... }`, which creates one
`AutoCloseable` per class and session and closes it on disposal, and `scope.repeatEvery(1.seconds) { ... }` with Kotlin
durations. `scope.own(task)` and `scope.own(player, task)` attach an existing Minestom `Task` to session or player
cleanup. These work with tasks created through KotStom; its event extensions can operate directly on `scope.events`.
Register resources and tasks on the tick thread.

Put final result mutations in `finish()`: the manager awaits that hook before closing session resources. Request
termination with `scope.finish()` without awaiting it from work that the same termination will cancel. Slow Flow
collectors fail at 64 queued updates instead of dropping stale transitions. Both languages use the same
`SessionProvider.create()` registration contract. The [Java consumer](../../examples/java/README.md) demonstrates the
Java lifecycle and generated client without Kotlin dependencies. The coroutine adapters live in `jvm:runtime-minestom-kotlin`;
backend-only Java consumers use `jvm:backend-client`.
