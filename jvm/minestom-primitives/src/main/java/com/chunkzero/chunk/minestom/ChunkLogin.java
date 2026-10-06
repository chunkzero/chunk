package com.chunkzero.chunk.minestom;

import com.chunkzero.chunk.runtime.ChunkSessions;
import com.chunkzero.chunk.runtime.Delivery;
import com.chunkzero.chunk.runtime.PlayerProfile;

import net.kyori.adventure.text.Component;
import net.minestom.server.MinecraftServer;
import net.minestom.server.entity.Player;
import net.minestom.server.event.Event;
import net.minestom.server.event.EventNode;
import net.minestom.server.event.player.AsyncPlayerPreLoginEvent;
import net.minestom.server.network.ConnectionState;
import net.minestom.server.network.player.GameProfile;
import net.minestom.server.network.player.PlayerConnection;
import net.minestom.server.timer.Task;
import net.minestom.server.timer.TaskSchedule;

import java.util.Map;
import java.util.Objects;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionStage;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.TimeUnit;
import java.util.function.BiFunction;

/**
 * Admits the players core delivers to a Minestom server whose app runs its own sessions through
 * {@link ChunkSessions}. Add {@link #events()} to the server's event handler: each login answers
 * the gateway's {@code chunk:delivery} request, and is refused unless it presents a delivery this
 * JVM prepared. Where the player spawns, and when they {@linkplain Delivery#arrived() arrive}, is
 * up to the app.
 *
 * <p>Once a player's connection closes, whether they left or their delivery was withdrawn, it runs
 * the app's leave handler, and releases the delivery once that completes successfully. A leave that
 * fails is logged and its delivery stays unreleased.
 */
public final class ChunkLogin implements AutoCloseable {
    private static final System.Logger LOG = System.getLogger(ChunkLogin.class.getName());

    private final ChunkSessions sessions;
    private final BiFunction<Player, Delivery, CompletionStage<Void>> leave;
    private final EventNode<Event> events = EventNode.all("chunk-login");
    private final Map<PlayerConnection, Admission> admissions = new ConcurrentHashMap<>();
    private final Task sweep;

    private ChunkLogin(
            ChunkSessions sessions, BiFunction<Player, Delivery, CompletionStage<Void>> leave) {
        this.sessions = Objects.requireNonNull(sessions);
        this.leave = Objects.requireNonNull(leave);
        events.addListener(AsyncPlayerPreLoginEvent.class, this::preLogin);
        sweep =
                MinecraftServer.getSchedulerManager()
                        .buildTask(this::sweep)
                        .repeat(TaskSchedule.tick(1))
                        .schedule();
    }

    /** Admits delivered players, and releases each once their connection closes. */
    public static ChunkLogin create(ChunkSessions sessions) {
        return new ChunkLogin(
                sessions, (player, delivery) -> CompletableFuture.completedFuture(null));
    }

    /**
     * Admits delivered players. Once a player's connection closes, {@code leave} runs, and their
     * delivery is released once it completes successfully, so the session can still act for them
     * meanwhile.
     */
    public static ChunkLogin create(
            ChunkSessions sessions, BiFunction<Player, Delivery, CompletionStage<Void>> leave) {
        return new ChunkLogin(sessions, leave);
    }

    /** The login listeners; add them to the server's event handler. */
    public EventNode<Event> events() {
        return events;
    }

    /**
     * The delivery {@code player} was admitted under.
     *
     * @throws IllegalArgumentException if core did not deliver the player, or they were released
     */
    public Delivery delivery(Player player) {
        var admission = admissions.get(player.getPlayerConnection());
        if (admission == null) throw new IllegalArgumentException("Player was not delivered");
        return admission.delivery;
    }

    /** Stops releasing players whose connection closed. */
    @Override
    public void close() {
        sweep.cancel();
    }

    private void preLogin(AsyncPlayerPreLoginEvent event) {
        var connection = event.getConnection();
        try {
            var payload =
                    event.sendPluginRequest("chunk:delivery", new byte[0])
                            .get(5, TimeUnit.SECONDS)
                            .payload();
            var presented = event.getGameProfile();
            var delivery =
                    sessions.admit(
                            payload, presented.uuid(), presented.name(), connection::disconnect);
            admissions.put(connection, new Admission(delivery));
            event.setGameProfile(profile(delivery.player()));
        } catch (Exception ignored) {
            connection.kick(Component.text("Delivery rejected"));
        }
    }

    private void sweep() {
        admissions.forEach(
                (connection, admission) -> {
                    if (connection.isOnline() || admission.leaving) return;
                    var player = connection.getPlayer();
                    // Minestom removes a PLAY player's entity on a later tick; a player that left
                    // during configuration is never removed.
                    if (player != null
                            && connection.getServerState() == ConnectionState.PLAY
                            && !player.isRemoved()) return;
                    admission.leaving = true;
                    CompletionStage<Void> left;
                    try {
                        left =
                                player == null
                                        ? CompletableFuture.completedFuture(null)
                                        : leave.apply(player, admission.delivery);
                    } catch (RuntimeException error) {
                        left = CompletableFuture.failedFuture(error);
                    }
                    left.whenComplete(
                            (ignored, error) -> {
                                if (error != null) {
                                    LOG.log(
                                            System.Logger.Level.WARNING,
                                            "Player leave failed; the delivery stays unreleased",
                                            error);
                                    return;
                                }
                                admissions.remove(connection, admission);
                                admission.delivery.release();
                            });
                });
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

    private static final class Admission {
        final Delivery delivery;
        // Only the scheduler thread reads and writes it.
        boolean leaving;

        Admission(Delivery delivery) {
            this.delivery = delivery;
        }
    }
}
