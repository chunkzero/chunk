package com.chunkzero.chunk.runtime.minestom.internal;

import com.chunkzero.chunk.runtime.Delivery;
import com.chunkzero.chunk.runtime.ManagedPlayer;
import com.chunkzero.chunk.runtime.SessionManager;

import net.minestom.server.instance.Instance;
import net.minestom.server.network.player.PlayerConnection;

import org.jetbrains.annotations.Nullable;

import java.util.concurrent.CompletableFuture;

/**
 * A connection admitted under a delivery: it joins the delivery's session once spawned, arrives
 * once its client confirmed its position, and is released once it closed and left the session.
 */
final class AdmittedPlayer {
    private static final System.Logger LOG = System.getLogger(AdmittedPlayer.class.getName());

    private final PlayerConnection connection;
    private final SessionManager manager;
    private final CompletableFuture<Delivery> delivery = new CompletableFuture<>();
    private final CompletableFuture<Void> released = new CompletableFuture<>();
    private @Nullable SessionManager.ManagedSession session;
    private @Nullable ManagedPlayer player;
    private CompletableFuture<Void> joining = CompletableFuture.completedFuture(null);
    private boolean joinStarted;
    private boolean closed;

    AdmittedPlayer(PlayerConnection connection, SessionManager manager) {
        this.connection = connection;
        this.manager = manager;
    }

    void admitted(Delivery admitted) {
        delivery.complete(admitted);
    }

    boolean isReleased() {
        return released.isDone();
    }

    synchronized Instance configure(ManagedPlayer current) {
        var admitted = delivery.getNow(null);
        if (closed || admitted == null || connection != current.getPlayerConnection())
            throw new IllegalStateException("Delivery closed");
        var managed = manager.get(admitted.session());
        current.setDelivery(admitted);
        session = managed;
        player = current;
        return managed.getScope().getInstances().getFirst();
    }

    /** Joins the spawned player to the session, reports arrival, and closes once offline. */
    synchronized void check() {
        if (closed) return;
        if (!connection.isOnline()) {
            close();
            return;
        }
        if (player == null || session == null) return;
        var initialization = player.getInitialization();
        var spawned =
                initialization != null
                        && initialization.isDone()
                        && !initialization.isCompletedExceptionally();
        if (!spawned) return;
        if (!joinStarted) {
            joinStarted = true;
            joining = session.join(player);
            joining.whenComplete(
                    (ignored, error) -> {
                        if (error != null) close();
                    });
        }
        if (joining.isDone()
                && !joining.isCompletedExceptionally()
                && player.getLastSentTeleportId() > 0
                && player.getLastReceivedTeleportId() == player.getLastSentTeleportId())
            delivery.join().arrived();
    }

    /**
     * Disconnects the player, awaits their join and spawn, removes them, runs the session's leave,
     * and then releases the delivery.
     */
    synchronized CompletableFuture<Void> close() {
        if (closed) return released;
        closed = true;
        connection.disconnect();
        var ticks = manager.getTicks();
        var current = player;
        var joined = session;
        joining.handle((ignored, error) -> null)
                .thenCompose(
                        ignored ->
                                ticks.submit(
                                                () -> {
                                                    var initialization =
                                                            current == null
                                                                    ? null
                                                                    : current.getInitialization();
                                                    return initialization == null
                                                            ? CompletableFuture
                                                                    .<Void>completedFuture(null)
                                                            : initialization;
                                                })
                                        .thenCompose(
                                                initialization ->
                                                        initialization.handle(
                                                                (result, error) -> null)))
                .thenCompose(
                        ignored ->
                                ticks.submit(
                                        () -> {
                                            if (current != null) {
                                                connection
                                                        .process()
                                                        .connectionManager()
                                                        .removePlayer(connection);
                                                if (current.getInstance() != null
                                                        && !current.isRemoved()) current.remove();
                                            }
                                            return null;
                                        }))
                .thenCompose(
                        ignored ->
                                current == null || joined == null
                                        ? CompletableFuture.<Void>completedFuture(null)
                                        : joined.leave(current))
                .whenComplete(
                        (ignored, error) -> {
                            if (error != null)
                                LOG.log(
                                        System.Logger.Level.WARNING,
                                        "Player leave did not complete cleanly",
                                        error);
                            synchronized (this) {
                                player = null;
                                session = null;
                            }
                            delivery.thenAccept(Delivery::release);
                            released.complete(null);
                        });
        return released;
    }
}
