package example.arena;

import java.util.HashMap;
import java.util.Map;

/**
 * Moves in flight, by player. An accepted move normally takes the player away; if the gateway
 * abandons it instead, the player is still here once it expires, and gets another attempt.
 */
final class PendingMoves<K> {
    private final int expirySeconds;
    private final Map<K, Integer> started = new HashMap<>();

    PendingMoves(int expirySeconds) {
        this.expirySeconds = expirySeconds;
    }

    /**
     * Whether to send a move for {@code key} at second {@code now}; if so, it counts as pending.
     */
    boolean start(K key, int now) {
        var since = started.get(key);
        if (since != null && now - since < expirySeconds) return false;
        started.put(key, now);
        return true;
    }

    /** A refused or failed move can be tried again at once. */
    void refused(K key) {
        started.remove(key);
    }

    void forget(K key) {
        started.remove(key);
    }
}
