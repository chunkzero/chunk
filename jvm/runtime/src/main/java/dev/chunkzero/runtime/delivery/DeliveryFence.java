package dev.chunkzero.runtime.delivery;

import java.util.HashMap;
import java.util.Map;

/** Retains fencing after disconnect; a player stream is never replayed. */
final class DeliveryFence {
    private record Owner(long generation, boolean active) {}

    private final Map<String, Owner> owners = new HashMap<>();

    synchronized void claim(String player, long generation) {
        if (player.chars()
                        .allMatch(
                                character ->
                                        Character.isWhitespace(character)
                                                || Character.isSpaceChar(character))
                || generation <= 0) {
            throw new IllegalArgumentException(
                    "Player identity and a positive delivery generation are required");
        }
        var previous = owners.get(player);
        if (previous != null && generation <= previous.generation()) {
            throw new IllegalArgumentException("Stale delivery generation");
        }
        if (previous != null && previous.active())
            throw new IllegalArgumentException("Player already delivered");
        if (previous == null && owners.size() >= 65_536) {
            throw new IllegalStateException("Process delivery history capacity reached");
        }
        owners.put(player, new Owner(generation, true));
    }

    synchronized void release(String player, long generation) {
        var owner = owners.get(player);
        if (owner != null && owner.generation() == generation)
            owners.put(player, new Owner(generation, false));
    }
}
