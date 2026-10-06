# Minestom primitives

Login handling for Minestom apps that run their own sessions through
[`ChunkSessions`](../runtime/README.md#running-your-own-sessions) instead of `runtime-minestom`. It compiles against
upstream [Minestom](https://minestom.net) (`net.minestom:minestom`) and leaves the Minestom dependency to the app, so
the app picks its Minestom build; it must speak the gateway's Minecraft version, Java Edition 26.2.

`ChunkLogin` answers the gateway's `chunk:delivery` login request, admits only players this JVM was asked to admit, with
the profile the gateway authenticated, and releases each delivery once its player's connection closed. Everything else,
such as what a session is, where a player spawns and when they've arrived, stays with the app:

```java
try (var chunk = ChunkProcess.connect()) {
    MinecraftServer.setCompressionThreshold(0); // the gateway owns compression
    var server = MinecraftServer.init();
    var sessions = chunk.host(games); // your SessionHandler
    var login = ChunkLogin.create(sessions, (player, delivery) -> games.leave(player));
    var events = MinecraftServer.getGlobalEventHandler();
    events.addChild(login.events());
    events.addListener(AsyncPlayerConfigurationEvent.class, event -> {
        var delivery = login.delivery(event.getPlayer());
        event.setSpawningInstance(games.spawn(delivery.session(), event.getPlayer()));
    });
    events.addListener(PlayerSpawnEvent.class, event -> {
        if (event.isFirstSpawn()) login.delivery(event.getPlayer()).arrived();
    });
    MinecraftServer.getSchedulerManager()
            .buildTask(chunk::tick)
            .repeat(TaskSchedule.tick(1))
            .schedule();
    server.start(new InetSocketAddress(chunk.playerAddress(), 0));
    chunk.bind(MinecraftServer.getServer().getPort(), MinecraftServer.PROTOCOL_VERSION);
    chunk.ready();
    chunk.awaitShutdown();
}
```

- Keep Minestom in offline mode: the gateway authenticates players and owns encryption and compression.
- The leave handler runs once a player's connection closed, whether they left or core withdrew their delivery, and the
  delivery is released when it settles. A session ends only after its players are released.
- `login.delivery(player)` also gives `move(destination)` and `operationId(action)` for that player.
- Isolation is the app's choice: several sessions can share instances, own several each, or the app can run one session
  per JVM through its machine profile's `max_sessions = 1`.
