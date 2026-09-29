# Minestom runtime

The session API gameplay code is written against, and the adapter that runs it on [Minestom](https://minestom.net).
`ChunkMinestom` attaches an app-owned Minestom `ServerProcess` to a [`ChunkProcess`](../runtime/README.md), creates
sessions when core asks, admits the players core delivers, and runs session methods. `jvm/runtime-minestom-kotlin` adds
coroutine adapters, described [below](#kotlin).

Minestom comes from [chunkzero/minestom-me](https://github.com/chunkzero/minestom-me), published as
`net.minestom:minestom:master-SNAPSHOT` in `https://maven.chunkzero.com/snapshots`. It and the gateway target Minecraft
Java Edition 26.2.

## Sessions

A session is one gameplay instance of a session type, and one JVM runs several. Each session type is a `SessionProvider`
annotated with its ID, which must match an implementation in the app's `app.ts` (`default` unless it declares others).
The provider creates fresh state for every session. From the Java project template:

```java
@SessionType("default")
public final class Lobby implements SessionProvider {
    @Override
    public Session create() {
        return new GreetingSession();
    }

    private static final class GreetingSession extends Session {
        private SessionScope scope;
        private BackendClient backend;

        @Override
        public CompletionStage<Void> onCreate(SessionScope scope) {
            this.scope = scope;
            backend = new BackendClient(Objects.requireNonNull(scope.getBackend()));
            scope.createInstance()
                    .setGenerator(unit -> unit.modifier().fillHeight(0, 40, Block.GRASS_BLOCK));
            return CompletableFuture.completedFuture(null);
        }

        @Override
        public CompletionStage<Void> onJoin(Player player) {
            CompletableFuture<MessageResult> message =
                    backend.shared().greetings().message(new MessageArgs(player.getUsername()));
            return message.thenCompose(
                    result ->
                            scope.onTick(
                                    () -> player.sendMessage(Component.text(result.message()))));
        }
    }
}
```

The `main` method that starts the process is shown in the [runtime README](../runtime/README.md#process-lifecycle).

An implementation with a `config` validator in `app.ts` implements its generated interface instead and receives the
validated value, as the arena in the
[local example](../../examples/local/apps/games/arena/src/main/kotlin/dev/chunkzero/example/arena/ArenaSessions.kt)
does:

```kotlin
@SessionType("default")
class ArenaSessions : ArenaSessionProviders.Default {
    override fun create(creation: SessionCreation<SessionConfigs.Arena.Default.Config>) =
        ExampleSessions.arena("${creation.config().label()} (${creation.maxPlayers()} slots)")
}
```

### Lifecycle

`Session` has four hooks, each returning a `CompletionStage<Void>` and starting on the process tick thread. Never block
that thread; continue asynchronous work on it with `scope.onTick(...)`.

- `onCreate(scope)` builds the session. The session accepts players once the stage completes and it owns at least one
  instance.
- `onJoin(player)` runs after Minestom has spawned the player, so it can send UI and start backend calls. The player
  counts as arrived once the stage completes and the client acknowledges its position.
- `onLeave(player)` runs when a player leaves or moves elsewhere. The player stays the backend caller until it settles,
  so saving on leave works.
- `onFinish()` runs when the session ends and is awaited before its resources close; put final result writes here.

`scope.finish()` asks for the session to end. Don't await it from a hook that ending itself waits for.

### Session scope

`SessionScope` owns everything a session creates and cleans it up when the session ends:

| Method                                            | Use                                                                                            |
| ------------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| `createInstance()`, `getInstances()`              | Worlds owned by the session, unregistered with their entities at the end                       |
| `getEvents()`                                     | This session's event node: its lifecycle events, its players' events and its instances' events |
| `getScheduler()`, `repeatEvery(interval, action)` | Tasks on the tick thread, cancelled at the end                                                 |
| `onTick(action)`                                  | Run code on the tick thread from any thread                                                    |
| `own(closeable)`, `own(player, closeable)`        | Close a resource when the session ends, or when that player leaves                             |
| `resource(type, factory)`                         | One owned resource per class, created on first use                                             |
| `component(type)`                                 | A declared [component](#components)                                                            |
| `getBackend()`                                    | The session's backend client                                                                   |
| `operationId(player, action)`                     | A stable mutation ID for one action on this player's current delivery                          |
| `finish()`                                        | Ask for the session to end                                                                     |
| `getProcess()`                                    | The Minestom process shared by the app's sessions                                              |

Scope methods run on the tick thread. Sessions share the process's memory, threads and failures: a scope cleans up after
a session but does not isolate it. Anything registered directly on the process needs its own cleanup. A process runs at
most 256 sessions.

### Backend calls

`scope.getBackend()` calls core as this session. Wrap it in the generated `BackendClient` (see
[backend-client](../backend-client/README.md)) and resume on the tick thread before touching the world, as `onJoin`
above does. For calls that name a player as the caller, bind a child client and let the player's departure close it:

```java
var playerBackend =
        new BackendClient(
                scope.own(player, scope.getBackend().forPlayer(new PlayerId(player.getUuid().toString()))));
```

Mutations need an `OperationId`. `scope.operationId(player, "coin-" + sequence)` gives one that stays the same for that
action during the player's current delivery; retry an uncertain mutation with the same ID and arguments.

## Components

Declare shared dependencies as public static `@Component` factories instead of wiring them by hand. The return type is
the component's identity and the parameters are its dependencies. From the
[Java example](../../examples/java/apps/lobby/src/main/java/example/LobbyComponents.java):

```java
public final class LobbyComponents {
    private LobbyComponents() {}

    @Component(Component.Scope.SESSION)
    public static BackendClient backend(BackendSession session) {
        return new BackendClient(session);
    }
}
```

Gameplay code gets it with `scope.component(BackendClient.class)` on the tick thread. `SESSION` components are created
once per session and may take `SessionScope`, its `BackendSession` and other components. `PROCESS` components are shared
by the app's sessions and may depend only on other process components. Factories run lazily; results that are
`AutoCloseable` are closed in reverse creation order when their session ends or the process stops, and a failed factory
closes only what that lookup created. Factories must be non-generic, take their dependencies as parameters, and return
values they own. In Kotlin, use top-level functions or `@JvmStatic` functions in objects. The
[Gradle plugin](../gradle-plugin/README.md#components) checks the graph at build time and generates direct calls.

## Events

`dev.chunkzero.runtime.minestom.event` has one event per lifecycle step. Listen on the process to see every session, or
on `scope.getEvents()` for one:

```java
server.eventHandler().addListener(SessionJoinEvent.class, event -> {
    System.out.printf("%s joined session %s%n",
            event.getPlayer().getUsername(), event.getSession().getId());
});
```

| Event                 | Fires when                                                        |
| --------------------- | ----------------------------------------------------------------- |
| `SessionCreateEvent`  | Creation succeeded and the session has an instance                |
| `SessionJoinEvent`    | A player's `onJoin` succeeded                                     |
| `SessionLeaveEvent`   | A joined player left and `onLeave` settled                        |
| `SessionDestroyEvent` | The session's cleanup finished, including after a failed creation |

Events are synchronous on the tick thread and cannot be cancelled. Listen to the concrete classes: Minestom does not
deliver them to a listener on the `SessionEvent` interface.

## Kotlin

`jvm/runtime-minestom-kotlin` lets sessions use coroutines. Extend `CoroutineSession` and override its suspending
`create`, `join`, `leave` and `finish`. `scope.coroutines` (import `dev.chunkzero.runtime.coroutines`) is a
`CoroutineScope` owned by the session and dispatched on the tick thread, so `launch`, `async` and `Flow.launchIn` resume
there and are cancelled when the session ends. From the Kotlin project template:

```kotlin
private class GreetingSession : CoroutineSession() {
    private lateinit var scope: SessionScope

    override suspend fun create(scope: SessionScope) {
        this.scope = scope
        scope.createInstance().setGenerator { it.modifier().fillHeight(0, 40, Block.GRASS_BLOCK) }
    }

    override suspend fun join(player: Player) {
        val backend = CoroutineBackendClient(scope.coroutines.backend(requireNotNull(scope.backend), player))
        val result = backend.shared.greetings.message(MessageArgs(player.username))
        player.sendMessage(Component.text(result.message()))
    }
}
```

`scope.coroutines.backend(client)` and `backend(client, player)` return a `CoroutineBackend` for suspending calls and
`Flow` watches, closed with the session or when the player leaves. A watch collector that falls 64 updates behind fails
instead of silently dropping states. The module also adds `scope.resource<T> { ... }`, `scope.own(task)`,
`scope.own(player, task)` for Minestom tasks, and `scope.repeatEvery(1.seconds) { ... }` with Kotlin durations.
