package com.chunkzero.chunk.runtime.minestom.internal;

import chunk.sync.v1.CoreOuterClass.Position;

import java.util.HashMap;
import java.util.Map;

/**
 * Admits each player under one delivery at a time, each newer than the last. It retains fencing
 * after disconnect, so a player's stale delivery is never replayed.
 */
final class DeliveryFence {
    private record Owner(Position generation, boolean active) {}

    private final Map<String, Owner> owners = new HashMap<>();

    synchronized void claim(String player, Position generation) {
        if (player.isBlank() || generation.getEpoch() < 1 || generation.getRevision() < 1) {
            throw new IllegalArgumentException(
                    "Player identity and a delivery generation are required");
        }
        var previous = owners.get(player);
        if (previous != null && !before(previous.generation(), generation)) {
            throw new IllegalArgumentException("Stale delivery generation");
        }
        if (previous != null && previous.active())
            throw new IllegalArgumentException("Player already delivered");
        if (previous == null && owners.size() >= 65_536) {
            throw new IllegalStateException("Process delivery history capacity reached");
        }
        owners.put(player, new Owner(generation, true));
    }

    synchronized void release(String player, Position generation) {
        var owner = owners.get(player);
        if (owner != null && owner.generation().equals(generation))
            owners.put(player, new Owner(generation, false));
    }

    private static boolean before(Position first, Position second) {
        return first.getEpoch() < second.getEpoch()
                || (first.getEpoch() == second.getEpoch()
                        && first.getRevision() < second.getRevision());
    }
}
