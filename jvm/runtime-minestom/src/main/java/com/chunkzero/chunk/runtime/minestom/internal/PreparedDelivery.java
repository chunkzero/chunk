package com.chunkzero.chunk.runtime.minestom.internal;

import chunk.sync.v1.Gateway.PlayerIdentity;
import chunk.sync.v1.Jvm.JvmDelivery;
import chunk.sync.v1.Jvm.JvmDeliveryPhase;
import chunk.sync.v1.Jvm.JvmDeliveryStatus;
import chunk.sync.v1.Jvm.JvmSessionPhase;
import chunk.sync.v1.Jvm.PlayerSetup;

import com.chunkzero.chunk.runtime.ManagedPlayer;
import com.chunkzero.chunk.runtime.SessionManager;
import com.chunkzero.chunk.runtime.TickExecutor;
import com.google.protobuf.ByteString;

import net.minestom.server.instance.Instance;
import net.minestom.server.network.ConnectionState;
import net.minestom.server.network.player.GameProfile;
import net.minestom.server.network.player.PlayerConnection;

import org.jetbrains.annotations.Nullable;

import java.security.MessageDigest;
import java.security.SecureRandom;
import java.util.Objects;
import java.util.UUID;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import java.util.function.LongSupplier;

/**
 * A delivery the JVM prepared: a slot in its session and the capability it minted, which admits the
 * player once. Once closed it holds no live player references.
 */
final class PreparedDelivery {
    /**
     * How long the gateway may take to connect the player: it loads their resource packs first.
     * Control cancels the claim of a delivery not activated within 60 seconds of its preparation
     * anyway.
     */
    private static final long CONNECT_NANOS = TimeUnit.SECONDS.toNanos(60);

    private final String operation;
    private final JvmDelivery delivery;
    private final DeliveryFence owners;
    private final LongSupplier now;
    private final SessionManager.ManagedSession session;
    private final TickExecutor ticks;
    private final byte[] capability = new byte[32];
    private final long openedAt;
    private boolean consumed;
    private boolean closed;
    private @Nullable PlayerConnection connection;
    private @Nullable ManagedPlayer player;
    private CompletableFuture<Void> joining = CompletableFuture.completedFuture(null);
    private final CompletableFuture<Void> removed = new CompletableFuture<>();
    private boolean arrived;
    private boolean joinStarted;
    private @Nullable JvmDeliveryStatus status;

    PreparedDelivery(
            String operation,
            JvmDelivery delivery,
            DeliveryFence owners,
            LongSupplier now,
            SessionManager.ManagedSession session,
            TickExecutor ticks) {
        this.operation = operation;
        this.delivery = delivery;
        this.owners = owners;
        this.now = now;
        this.session = session;
        this.ticks = ticks;
        new SecureRandom().nextBytes(capability);
        openedAt = now.getAsLong();
    }

    JvmDelivery getDelivery() {
        return delivery;
    }

    /** Whether the player is in {@code id}, the delivery's session. */
    synchronized boolean arrivedIn(String id) {
        return !closed
                && arrived
                && player != null
                && player.isOnline()
                && connection != null
                && connection.isOnline()
                && session.getPhase() == JvmSessionPhase.JVM_SESSION_PHASE_READY
                && delivery.getSession().equals(id);
    }

    synchronized boolean owns(PlayerConnection current) {
        return !closed && connection == current;
    }

    synchronized boolean isReleased() {
        return removed.isDone() && !removed.isCompletedExceptionally();
    }

    synchronized Instance configure(ManagedPlayer current) {
        if (closed || connection != current.getPlayerConnection())
            throw new IllegalStateException("Delivery closed");
        if (session.getPhase() != JvmSessionPhase.JVM_SESSION_PHASE_READY)
            throw new IllegalStateException("Session unavailable");
        current.setBinding(operation, delivery);
        player = current;
        return session.getScope().getInstances().getFirst();
    }

    synchronized JvmDeliveryStatus status() {
        checkDeadline();
        JvmDeliveryPhase phase;
        if (closed && !isReleased()) phase = JvmDeliveryPhase.JVM_DELIVERY_PHASE_WITHDRAWING;
        else if (closed) phase = JvmDeliveryPhase.JVM_DELIVERY_PHASE_CLOSED;
        else if (arrived) phase = JvmDeliveryPhase.JVM_DELIVERY_PHASE_ARRIVED;
        else if (connection != null && connection.getClientState() == ConnectionState.PLAY) {
            phase = JvmDeliveryPhase.JVM_DELIVERY_PHASE_ATTACHED;
        } else phase = JvmDeliveryPhase.JVM_DELIVERY_PHASE_PREPARED;
        if (status == null || status.getPhase() != phase) {
            var next =
                    JvmDeliveryStatus.newBuilder()
                            .setOperationId(operation)
                            .setGeneration(delivery.getGeneration())
                            .setPhase(phase);
            if (phase == JvmDeliveryPhase.JVM_DELIVERY_PHASE_PREPARED)
                next.setCapability(ByteString.copyFrom(capability));
            status = next.build();
        }
        return status;
    }

    synchronized GameProfile consume(
            PlayerSetup setup, GameProfile presented, PlayerConnection accepted) {
        checkDeadline();
        if (closed || consumed || session.getPhase() != JvmSessionPhase.JVM_SESSION_PHASE_READY) {
            throw new IllegalStateException("Delivery unavailable");
        }
        if (!setup.getOperationId().equals(operation)
                || !MessageDigest.isEqual(capability, setup.getCapability().toByteArray())) {
            throw new IllegalArgumentException("Invalid delivery capability");
        }
        var profile = profile(delivery.getPlayer());
        if (!presented.uuid().equals(profile.uuid()) || !presented.name().equals(profile.name())) {
            throw new IllegalArgumentException("Player identity mismatch");
        }
        owners.claim(delivery.getPlayer().getUuid(), delivery.getGeneration());
        consumed = true;
        connection = accepted;
        return profile;
    }

    synchronized void checkDeadline() {
        if (player != null) {
            var initialization = player.getInitialization();
            var spawned =
                    initialization != null
                            && initialization.isDone()
                            && !initialization.isCompletedExceptionally();
            if (spawned && !closed && !joinStarted) {
                joinStarted = true;
                joining = session.join(player);
                joining.whenComplete(
                        (ignored, error) -> {
                            if (error != null) close();
                        });
            }
            if (spawned
                    && joinStarted
                    && joining.isDone()
                    && !joining.isCompletedExceptionally()
                    && player.getLastSentTeleportId() > 0
                    && player.getLastReceivedTeleportId() == player.getLastSentTeleportId()) {
                arrived = true;
            }
        }
        if ((!consumed && now.getAsLong() - openedAt >= CONNECT_NANOS)
                || (connection != null && !connection.isOnline())) {
            close();
        }
    }

    synchronized CompletableFuture<Void> close() {
        if (closed) return removed;
        closed = true;
        var current = connection;
        if (current != null) current.disconnect();
        joining.handle((ignored, error) -> null)
                .thenCompose(
                        ignored ->
                                ticks.submit(
                                                () -> {
                                                    var initialization =
                                                            player == null
                                                                    ? null
                                                                    : player.getInitialization();
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
                                            if (player != null) {
                                                Objects.requireNonNull(current)
                                                        .process()
                                                        .connectionManager()
                                                        .removePlayer(current);
                                                if (player.getInstance() != null
                                                        && !player.isRemoved()) player.remove();
                                            }
                                            return player;
                                        }))
                .thenCompose(
                        currentPlayer ->
                                currentPlayer == null
                                        ? CompletableFuture.completedFuture(null)
                                        : session.leave(currentPlayer))
                .whenComplete(
                        (ignored, error) -> {
                            synchronized (this) {
                                connection = null;
                                player = null;
                                if (error == null) {
                                    if (consumed)
                                        owners.release(
                                                delivery.getPlayer().getUuid(),
                                                delivery.getGeneration());
                                    removed.complete(null);
                                } else {
                                    removed.completeExceptionally(error);
                                }
                            }
                        });
        return removed;
    }

    private static GameProfile profile(PlayerIdentity identity) {
        return new GameProfile(
                UUID.fromString(identity.getUuid()),
                identity.getUsername(),
                identity.getPropertiesList().stream()
                        .map(
                                property ->
                                        new GameProfile.Property(
                                                property.getName(),
                                                property.getValue(),
                                                property.hasSignature()
                                                        ? property.getSignature()
                                                        : null))
                        .toList());
    }
}
