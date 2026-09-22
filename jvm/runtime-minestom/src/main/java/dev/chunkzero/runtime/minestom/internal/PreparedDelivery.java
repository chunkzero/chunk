package dev.chunkzero.runtime.minestom.internal;

import chunk.v1.Common.Identity;
import chunk.v1.GameplayOuterClass.PlayerDelivery;
import chunk.v1.GameplayOuterClass.PlayerPreparation;
import chunk.v1.GameplayOuterClass.PlayerSetup;
import chunk.v1.SessionMethodsOuterClass.SessionMethodRequest;
import chunk.v1.Supervision.DeliveryInventory;
import chunk.v1.Supervision.DeliveryPhase;
import chunk.v1.Supervision.SessionPhase;

import com.google.protobuf.ByteString;

import dev.chunkzero.runtime.ManagedPlayer;
import dev.chunkzero.runtime.SessionManager;
import dev.chunkzero.runtime.TickExecutor;

import net.minestom.server.instance.InstanceContainer;
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

/** Single-use admission record; terminal history retains no live player references. */
final class PreparedDelivery {
    private final PlayerDelivery delivery;
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

    PreparedDelivery(
            PlayerDelivery delivery,
            DeliveryFence owners,
            LongSupplier now,
            SessionManager.ManagedSession session,
            TickExecutor ticks) {
        this.delivery = delivery;
        this.owners = owners;
        this.now = now;
        this.session = session;
        this.ticks = ticks;
        new SecureRandom().nextBytes(capability);
        openedAt = now.getAsLong();
    }

    PlayerDelivery getDelivery() {
        return delivery;
    }

    synchronized boolean authorizes(SessionMethodRequest request) {
        var caller = request.getCaller();
        return !closed
                && arrived
                && player != null
                && player.isOnline()
                && connection != null
                && connection.isOnline()
                && session.getPhase() == SessionPhase.SESSION_PHASE_READY
                && delivery.getSession().equals(request.getSession())
                && delivery.getSessionGeneration() == request.getSessionGeneration()
                && delivery.getPlayer().equals(caller.getPlayer())
                && delivery.getMembershipGeneration() == caller.getMembershipGeneration()
                && delivery.getOwnerGeneration() == caller.getOwnerGeneration();
    }

    synchronized boolean owns(PlayerConnection current) {
        return !closed && connection == current;
    }

    synchronized boolean isReleased() {
        return removed.isDone() && !removed.isCompletedExceptionally();
    }

    synchronized InstanceContainer configure(ManagedPlayer current) {
        if (closed || connection != current.getPlayerConnection())
            throw new IllegalStateException("Delivery closed");
        if (session.getPhase() != SessionPhase.SESSION_PHASE_READY)
            throw new IllegalStateException("Session unavailable");
        current.setBinding(delivery);
        player = current;
        return session.getScope().getInstances().getFirst();
    }

    synchronized DeliveryInventory inventory() {
        checkDeadline();
        DeliveryPhase phase;
        if (closed && !isReleased()) phase = DeliveryPhase.DELIVERY_PHASE_WITHDRAWING;
        else if (closed) phase = DeliveryPhase.DELIVERY_PHASE_CLOSED;
        else if (arrived) phase = DeliveryPhase.DELIVERY_PHASE_ARRIVED;
        else if (connection != null && connection.getClientState() == ConnectionState.PLAY) {
            phase = DeliveryPhase.DELIVERY_PHASE_ATTACHED;
        } else phase = DeliveryPhase.DELIVERY_PHASE_PREPARED;
        return DeliveryInventory.newBuilder()
                .setDelivery(delivery.toBuilder().clearIdentity())
                .setPhase(phase)
                .build();
    }

    synchronized PlayerPreparation result(String endpoint) {
        checkDeadline();
        if (closed) throw new IllegalStateException("Delivery closed");
        return PlayerPreparation.newBuilder()
                .setOperationId(delivery.getOperationId())
                .setEndpoint(endpoint)
                .setCapability(ByteString.copyFrom(capability))
                .build();
    }

    synchronized GameProfile consume(
            PlayerSetup setup, GameProfile presented, PlayerConnection accepted) {
        checkDeadline();
        if (closed || consumed || session.getPhase() != SessionPhase.SESSION_PHASE_READY) {
            throw new IllegalStateException("Delivery unavailable");
        }
        if (!setup.getOperationId().equals(delivery.getOperationId())
                || !MessageDigest.isEqual(capability, setup.getCapability().toByteArray())) {
            throw new IllegalArgumentException("Invalid delivery capability");
        }
        var profile = profile(delivery.getIdentity());
        if (!presented.uuid().equals(profile.uuid()) || !presented.name().equals(profile.name())) {
            throw new IllegalArgumentException("Player identity mismatch");
        }
        owners.claim(delivery.getIdentity().getUuid(), delivery.getOwnerGeneration());
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
        if ((!consumed && now.getAsLong() - openedAt >= TimeUnit.SECONDS.toNanos(30))
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
                                                delivery.getIdentity().getUuid(),
                                                delivery.getOwnerGeneration());
                                    removed.complete(null);
                                } else {
                                    removed.completeExceptionally(error);
                                }
                            }
                        });
        return removed;
    }

    private static GameProfile profile(Identity identity) {
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
