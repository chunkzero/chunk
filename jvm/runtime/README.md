# Integrated server library

`jvm:runtime` is the engine-independent Java 25 library. `ChunkProcess` owns immutable deployment identity, backend
binding, registration, explicit readiness, health reporting and shutdown notification. It has no Minestom or Kotlin
production dependency. `jvm:runtime-minestom` supplies `ChunkMinestom`, sessions, players, worlds, tick scheduling and
Minecraft admission. `jvm:runtime-minestom-kotlin` adds coroutine conveniences to the Minestom integration. Hytale
integration is future work.

The adapter uses [chunkzero/minestom-me](https://github.com/chunkzero/minestom-me), currently published as
`net.minestom:minestom:master-SNAPSHOT`. Create an independent `ServerProcess` and pass it to `ChunkMinestom.attach`;
closing the integration stops only that Minestom process. The current snapshot and Chunk proxy target Minecraft 26.2.

Each app is an executable JAR containing its own dependencies. Configure `application.mainClass` in the app's Gradle
build:

```java
@SessionType("default")
public final class Lobby implements SessionProvider {
    public static void main(String[] args) throws Exception {
        try (var chunk = ChunkProcess.connect();
             var minestom = ChunkMinestom.attach(chunk, ServerProcess.create())) {
            // Initialize application resources here.
            minestom.start();
            chunk.ready();
            chunk.awaitShutdown();
        }
    }

    @Override public Session create() { return new LobbySession(); }
}
```

The build plugin scans compiled `@SessionType` factories and generates the local Java service registry at
`META-INF/services/dev.chunkzero.runtime.SessionProvider`. Factories implement `SessionProvider` with a public
no-argument constructor and create fresh state for every session. The JVM resolves its factories through `ServiceLoader`
and binds their caller identity to the app assigned at launch.

Deployment requirements live in `app.toml`: `[runtime]` supplies app defaults and `[sessions.large]` can override
`machine_profile` and `capacity` for `@SessionType("large")`. The release manifest contains the resolved requirements
and artifact hashes; control uses it to select the app and profile, verify the JAR, and send session creation commands
with the chosen capacity. The JVM enforces those commands without loading a deployment manifest. Changing deployment
requirements creates a new release while preserving the executable bytes.

The platform supplies `CHUNK_PROCESS_TOKEN`, `CHUNK_DEPLOYMENT`, `CHUNK_CORE_ENDPOINT` (read as `CHUNK_CONTROL_ENDPOINT`
when unset), `CHUNK_PROCESS_ID`, `CHUNK_PROCESS_GENERATION`, `CHUNK_MACHINE_PROFILE`, `CHUNK_ARTIFACT_DIGEST` and
`CHUNK_APP_ID`. There is no deployment fallback or standalone unbound fixture mode.

`ready()` registers the frozen process with core's `chunk:register` only after application initialization, retrying
while core is unavailable. Core checks that the deployment is active, and a refused registration fails `ready()`. The
JVM then follows its `jvm/<host>` topic for the sessions, deliveries and session methods to run and whether to stop, and
reports with `chunk:report` on a channel of its own: everything it holds on each new stream, then what changed, with its
health at least every three seconds. Core counts a JVM unhealthy once it has reported no health for ten seconds. Health
carries engine progress recorded by the tick thread, heap use, GC counters, CPU load and session/player counts, so a
responsive link alone does not establish engine health. A broken or superseded stream registers and subscribes again.

`shutdownRequested()` provides a completion stage; `awaitShutdown()` is the blocking main-thread equivalent.
`requestShutdown()` lets the app stop accepting work and notify its shutdown handler; core's `stop` entry requests it
too. The app closes engine resources when shutdown is requested. The host allows a bounded graceful shutdown, then kills
the owned process if necessary and waits for exit. Player ownership is released only after withdrawal or confirmed
process exit.

The proxy connects directly to Minestom's loopback Minecraft listener. Single-use `chunk:delivery` capabilities
authorize native login. There is no per-server Rust process or intermediate TCP relay. Minestom owns configuration,
socket buffering, player creation and worlds. The current host and engine adapter are local; hosted networking/providers
require additional implementations. Use `just local` to run the example.

The `dev.chunkzero.runtime` package retains the public app/session API. Generic process wiring lives in `bootstrap` and
`control`; Minestom wiring lives in `minestom.internal`. Cross-package implementation APIs are marked
`@ApiStatus.Internal`. Health reports active sessions; completed and failed sessions remain in inventory for replay.

Session implementations extend `Session`. `onCreate`, `onJoin`, `onLeave` and `onFinish` return `CompletionStage<Void>`
and begin on the process tick thread. Do not block that thread. Scope methods also accept the process's dispatcher
thread when it is the only one, so native instance and entity listeners can use them. Resume asynchronous world changes
with `scope.onTick(() -> ...)` in Java or `scope.onTick { ... }` in Kotlin. Creation becomes ready only after its stage
completes and at least one instance exists; admission, delivery and session methods reject sessions that are not ready.
`scope.finish()` requests ending; do not await it from a lifecycle hook whose own completion ending must await.

`SessionScope` is the session's gameplay isolate within the app's shared `ServerProcess`. It owns instances from
`createInstance`, with their entities and instance schedulers; tasks on `getScheduler()` and `repeatEvery`; its event
node; and registered `AutoCloseable` resources such as subscriptions. `getEvents()` receives the scope's lifecycle
notifications, its admitted players' events, and events of its instances and the entities in them. Direct process
registrations require explicit cleanup. Ending, including after failed creation, withdraws deliveries, waits for
leave/finish hooks, then cancels the scope's tasks, closes its resources, unregisters its instances and detaches its
node. Sessions share the process's memory, threads and failures; a scope is not a sandbox. A process runs at most 256
live sessions and holds at most 4096 deliveries at once. Its delivery fence remembers up to 65,536 players; exhausting
that history requires a replacement process. Stuck customer futures retain ownership until a host deadline terminates
the process; they never produce a false withdrawal acknowledgment.

## Components

Declare app-local dependencies with public static `@Component` factories. The app build generates direct calls and
validates exact return/parameter types, missing dependencies, duplicates, cycles, and process-to-session captures.
Factories run lazily on the tick thread; constructor injection, qualifiers, and runtime class scanning are unsupported.

```java
public final class Services {
    @Component(Component.Scope.PROCESS)
    public static Rules rules() {
        return new Rules();
    }

    @Component(Component.Scope.SESSION)
    public static Scores scores(Rules rules, SessionScope scope) {
        return new Scores(rules, scope.getId());
    }
}

// In onCreate, onJoin, or scope.onTick:
Scores scores = scope.component(Scores.class);
```

Process components are shared by this app process; session components are cached independently for each session. Session
factories may request `SessionScope` and its bound `BackendSession` as borrowed parameters. Process factories cannot
depend on either capability or on session components. Use declared parameters for dependencies: factories and close
callbacks cannot re-enter component lookup or disposal. Factories must return their own values, never a borrowed session
capability or an already owned closeable. Do not register returned closeables with `scope.own` again.

The registry closes successful `AutoCloseable` results in reverse construction order. A failed factory rolls back only
the dependencies created for that lookup, keeping existing shared components available. Session disposal closes its
components before its backend client; process shutdown closes all remaining session components before shared process
components. Independent cleanup continues after a close failure, with errors retained as suppressed exceptions.
Factories should remain synchronous and short; application side effects outside owned resources are not rolled back.

Lifecycle notifications live in `dev.chunkzero.runtime.minestom.event`. `SessionEvent` exposes `getSession()`, the
owning `SessionScope` with its ID and generation. `SessionJoinEvent` and `SessionLeaveEvent` also implement Minestom's
`PlayerEvent`. Subscribe to concrete event classes on the process to observe all of its sessions:

```java
server.eventHandler().addListener(SessionJoinEvent.class, event -> {
    System.out.printf("%s joined session %s%n",
        event.getPlayer().getUsername(), event.getSession().getId());
});
```

Register on `scope.events` during `onCreate` to observe only that session. Its `EventNode<Event>` receives session
lifecycle events even after player membership has ended, alongside the existing admitted-player Minestom events.

| Event                 | Timing                                                                                  |
| --------------------- | --------------------------------------------------------------------------------------- |
| `SessionCreateEvent`  | Creation succeeded and instances are available.                                         |
| `SessionJoinEvent`    | The player's join hook succeeded, before control confirms arrival.                      |
| `SessionLeaveEvent`   | A joined player's membership ended and its leave hook settled, including failures.      |
| `SessionDestroyEvent` | Scope cleanup settled, before its node is detached; also emitted after failed creation. |

Notifications run synchronously on the process tick thread using Minestom's normal exception handling. They are
non-cancellable and do not replace the awaited lifecycle hooks. Failed admissions emit neither join nor leave events.
The scope is disposed during a destroy callback: its identity remains readable, but resources and new work are
unavailable. Minestom listeners target concrete classes; listening to the `SessionEvent` interface does not subscribe to
every implementation.

Core places each player on the JVM as a `delivery/<op>` entry. The JVM prepares it once its session is ready, reserving
capacity, and reports it PREPARED with a single-use capability it mints, which the gateway presents through the
`chunk:delivery` login plugin within 30 seconds. A player is admitted under one delivery at a time, each with a newer
claim generation than the last. Native Minecraft login attaches the player. `onJoin` begins after Minestom spawn
completes, so it can send player UI and start asynchronous backend work. Arrival requires the join stage and the latest
teleport acknowledgment. Withdrawing a delivery, or removing its key, fences output immediately, waits for pending
Minestom spawn callbacks and the join stage, removes the old player and completes its leave hook before releasing UUID
ownership and reporting the delivery CLOSED. Until then, app code may still name the player as its caller, so saves on
leave work. Session methods arrive as `method/<op>` entries: each runs once on the tick thread while its player is
arrived, unless cancelled or past its deadline first, and its result goes back through `chunk:method_result`. Managed
proxy moves prepare a new TCP delivery while the source plays, confirm withdrawal, drive both client configuration
acknowledgments, and activate the destination on the existing public connection.

The `scope.getBackend()` client (`scope.backend` in Kotlin) calls core over the sync protocol at `CHUNK_CORE_ENDPOINT`
with `CHUNK_PROCESS_TOKEN`, naming the process deployment, its session and optionally a player. Core derives the caller
app code sees, including the registered app ID, so function arguments do not choose that identity. The JVM requires an
explicit `CHUNK_DEPLOYMENT`, which core checks when the JVM registers. Missing configuration or an inactive deployment
fails startup. Use `scope.operationId(player, action)` for a mutation that should happen once per player delivery. It
returns a typed `OperationId`; retry an uncertain result with the same ID and arguments. Use
`scope.coroutines.backend(scope.backend, player)` for a player-bound client whose calls and watches close on departure.
Session clients close on disposal. The [local example](../../examples/local/README.md) demonstrates persistent coins,
visits and subscription updates, including stale state during backend disconnection. Sessions own their instances, event
handlers and scoped resources. Session hooks run through the process tick executor. Withdrawal waits for pending joins
and initialization, removes the player and runs its leave hook before releasing the ownership fence. Arrival is reported
after spawn and teleport acknowledgment.

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
Java lifecycle and generated client without Kotlin dependencies. The coroutine adapters live in
`jvm:runtime-minestom-kotlin`; backend-only Java consumers use `jvm:backend-client`.
