package com.chunkzero.chunk.runtime;

import chunk.sync.v1.CoreOuterClass.Position;
import chunk.sync.v1.Jvm.JvmDelivery;
import chunk.sync.v1.Jvm.JvmDeliveryPhase;
import chunk.sync.v1.Jvm.JvmDeliveryStatus;

import com.chunkzero.chunk.backend.api.Destination;
import com.chunkzero.chunk.backend.client.OperationId;
import com.google.protobuf.ByteString;

import org.jetbrains.annotations.Nullable;

import java.security.SecureRandom;
import java.util.Objects;
import java.util.UUID;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionStage;
import java.util.regex.Pattern;

/**
 * A player core placed in one of this JVM's sessions, from the moment {@link ChunkSessions#admit}
 * accepts their login until they are released. Its methods are thread-safe.
 */
public final class Delivery {
    private static final Pattern ACTION = Pattern.compile("[A-Za-z0-9_-]{1,32}");

    private final ChunkSessions owner;
    private final String id;
    private final JvmDelivery spec;
    private final PlayerProfile player;
    final byte[] capability = new byte[32];
    final long openedAt;
    final CompletableFuture<Void> released = new CompletableFuture<>();

    // Guarded by owner.
    boolean consumed;
    boolean arrived;
    boolean closed;
    boolean left;
    boolean isReleased;
    @Nullable Runnable disconnect;

    Delivery(ChunkSessions owner, String id, JvmDelivery spec, long openedAt) {
        this.owner = owner;
        this.id = id;
        this.spec = spec;
        this.openedAt = openedAt;
        var identity = spec.getPlayer();
        player =
                new PlayerProfile(
                        UUID.fromString(identity.getUuid()),
                        identity.getUsername(),
                        identity.getPropertiesList().stream()
                                .map(
                                        property ->
                                                new PlayerProfile.Property(
                                                        property.getName(),
                                                        property.getValue(),
                                                        property.hasSignature()
                                                                ? property.getSignature()
                                                                : null))
                                .toList());
        new SecureRandom().nextBytes(capability);
    }

    /** The claim's operation ID, as core names the delivery. */
    public String id() {
        return id;
    }

    /** The ID of the session the player joins. */
    public String session() {
        return spec.getSession();
    }

    /** Who the player authenticated as. */
    public PlayerProfile player() {
        return player;
    }

    /**
     * The player is in the session. Core then lets them run session methods and move. Ignored once
     * the delivery closed.
     */
    public void arrived() {
        owner.arrived(this);
    }

    public boolean isArrived() {
        synchronized (owner) {
            return arrived && !closed && !left;
        }
    }

    /**
     * The player's connection closed: no further session methods run for them. The delivery stays
     * fenced until {@link #release()}. Calling it again has no effect.
     */
    public void left() {
        owner.left(this);
    }

    /**
     * The player is gone and the session no longer acts for them. Call it once their connection
     * closed and any leave handling settled, whether they left or the delivery was withdrawn.
     */
    public void release() {
        owner.release(this);
    }

    /**
     * Asks core to move the player to {@code destination}, through its admission policy. Core
     * accepts only an arrived player's current delivery.
     */
    public CompletionStage<MoveResult> move(Destination destination) {
        return owner.move(this, Objects.requireNonNull(destination));
    }

    /** A mutation ID that stays the same for one action during this delivery. */
    public OperationId operationId(String action) {
        if (!ACTION.matcher(action).matches())
            throw new IllegalArgumentException("Invalid operation action");
        var generation = spec.getGeneration();
        return new OperationId(
                session()
                        + "/"
                        + player.uuid()
                        + "/"
                        + generation.getEpoch()
                        + "."
                        + generation.getRevision()
                        + "/"
                        + action);
    }

    Position generation() {
        return spec.getGeneration();
    }

    JvmDelivery spec() {
        return spec;
    }

    /** Called with the owner's lock held. */
    JvmDeliveryStatus status() {
        JvmDeliveryPhase phase;
        if (isReleased) phase = JvmDeliveryPhase.JVM_DELIVERY_PHASE_CLOSED;
        else if (closed) phase = JvmDeliveryPhase.JVM_DELIVERY_PHASE_WITHDRAWING;
        else if (arrived) phase = JvmDeliveryPhase.JVM_DELIVERY_PHASE_ARRIVED;
        else if (consumed) phase = JvmDeliveryPhase.JVM_DELIVERY_PHASE_ATTACHED;
        else phase = JvmDeliveryPhase.JVM_DELIVERY_PHASE_PREPARED;
        var status =
                JvmDeliveryStatus.newBuilder()
                        .setOperationId(id)
                        .setGeneration(spec.getGeneration())
                        .setPhase(phase);
        if (phase == JvmDeliveryPhase.JVM_DELIVERY_PHASE_PREPARED)
            status.setCapability(ByteString.copyFrom(capability));
        return status.build();
    }
}
