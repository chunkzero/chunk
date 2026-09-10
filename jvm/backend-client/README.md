# Java backend client

`BackendSession` binds an environment, immutable deployment, session/app/player
identity, call deadline and lifecycle. Construct it from trusted session ownership;
function arguments never modify caller identity. The channel and scheduler belong
to the parent runtime. Closing the session cancels its calls, watches and player
children; closing `forPlayer(...)` cancels only that child.

`chunk-codegen` also writes `java-client/<package>/BackendClient.java`. Add `java`
and `java-client` output directories to the application's generated source set.
Generated clients follow the backend namespace:

```java
var playerBackend = new BackendClient(playerSession);
playerBackend.shared().players().stats();
playerBackend.shared().players().coin(operation);
playerBackend.shared().players().watchStats(observer);
```

Calls return `CompletableFuture<R>`. Object arguments use records such as
`BackendTypes.Shared.Players.StatsArgs`; empty objects also have argument-free
overloads that send `{}`. Scalar and list results remain unwrapped.
Mutation methods require an `OperationId`: retain it with the request and reuse it
after an unknown outcome. Unary calls do not automatically retry. Cancelling the
future cancels the RPC; losing its reply does not prove the mutation failed.

Generated query `watchStats`-style methods return a closeable subscription and
deliver the complete `WatchState<R>`. Typed references such as
`BackendTypes.Shared.Players.stats` remain available for generic calls. For an
atomic heterogeneous group, bind references with `session.bind`, then use
`watchGroup` and retrieve each typed result with its original bound query object.
Per-query errors are values. Transport interruption retains the last snapshot
with `stale=true`; transient failures reconnect to a fresh complete group.
Observers are serialized and must return promptly. Scope closure prevents later
callbacks. Nontransient failures remain stale with an error until closed.

The public runtime classpath contains Java libraries only. The protobuf module
uses the Java convention plugin because it generates Java sources exclusively.
Focused check: `./gradlew :jvm:backend-client:test`.

Kotlin consumers can use `jvm:backend-client-kotlin` for `CoroutineBackend`,
which adds suspend calls and `Flow<WatchState<R>>` over the same Java client.
The adapter requires an owned coroutine scope and closes its calls and watches
when that scope ends.
