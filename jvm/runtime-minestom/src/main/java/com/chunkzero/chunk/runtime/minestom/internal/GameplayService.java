package com.chunkzero.chunk.runtime.minestom.internal;

import com.chunkzero.chunk.runtime.ChunkSessions;
import com.chunkzero.chunk.runtime.ManagedPlayer;
import com.chunkzero.chunk.runtime.PlayerProfile;
import com.chunkzero.chunk.runtime.SessionManager;

import net.kyori.adventure.text.Component;
import net.minestom.server.coordinate.Pos;
import net.minestom.server.event.Event;
import net.minestom.server.event.EventNode;
import net.minestom.server.event.player.AsyncPlayerConfigurationEvent;
import net.minestom.server.event.player.AsyncPlayerPreLoginEvent;
import net.minestom.server.network.player.GameProfile;
import net.minestom.server.network.player.PlayerConnection;

import org.jetbrains.annotations.ApiStatus;

import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.TimeUnit;

/**
 * Admits the players core delivers through the {@code chunk:delivery} login plugin and spawns each
 * in its session's first instance. A withdrawn delivery disconnects its player.
 */
@ApiStatus.Internal
public final class GameplayService {
    private final SessionManager manager;
    private final ChunkSessions sessions;
    private final EventNode<Event> events = EventNode.all("gameplay-delivery");
    private final Map<PlayerConnection, AdmittedPlayer> admitted = new ConcurrentHashMap<>();

    public GameplayService(SessionManager manager, ChunkSessions sessions) {
        this.manager = manager;
        this.sessions = sessions;
        events.addListener(AsyncPlayerPreLoginEvent.class, this::preLogin);
        events.addListener(AsyncPlayerConfigurationEvent.class, this::configure);
        manager.getProcess().eventHandler().addChild(events);
    }

    private void preLogin(AsyncPlayerPreLoginEvent event) {
        var connection = event.getConnection();
        var player = new AdmittedPlayer(connection, manager);
        try {
            var payload =
                    event.sendPluginRequest("chunk:delivery", new byte[0])
                            .get(5, TimeUnit.SECONDS)
                            .payload();
            var presented = event.getGameProfile();
            var delivery =
                    sessions.admit(payload, presented.uuid(), presented.name(), player::close);
            admitted.put(connection, player);
            player.admitted(delivery);
            event.setGameProfile(profile(delivery.player()));
        } catch (Exception ignored) {
            if (admitted.containsKey(connection)) player.close();
            connection.kick(Component.text("Delivery rejected"));
        }
    }

    private void configure(AsyncPlayerConfigurationEvent event) {
        try {
            var player = admitted.get(event.getPlayer().getPlayerConnection());
            if (player == null) throw new IllegalArgumentException("Unknown delivery");
            event.setSpawningInstance(player.configure((ManagedPlayer) event.getPlayer()));
            event.getPlayer().setRespawnPoint(new Pos(0.5, 42, 0.5));
        } catch (Exception ignored) {
            event.getPlayer().kick(Component.text("Session unavailable"));
        }
    }

    /** Advances admitted players, and forgets those released. Runs every tick. */
    public void flush() {
        admitted.values().forEach(AdmittedPlayer::check);
        admitted.values().removeIf(AdmittedPlayer::isReleased);
    }

    public void close() {
        manager.getProcess().eventHandler().removeChild(events);
        admitted.values().forEach(AdmittedPlayer::close);
    }

    private static GameProfile profile(PlayerProfile player) {
        return new GameProfile(
                player.uuid(),
                player.name(),
                player.properties().stream()
                        .map(
                                property ->
                                        new GameProfile.Property(
                                                property.name(),
                                                property.value(),
                                                property.signature()))
                        .toList());
    }
}
