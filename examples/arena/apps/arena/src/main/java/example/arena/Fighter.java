package example.arena;

import java.util.Optional;
import java.util.UUID;

/** One participant in the current match, kept by player ID while they come and go. */
final class Fighter {
    private static final long KILL_CREDIT_MILLIS = 10_000;

    final UUID id;
    final String name;
    final Team team;
    int captures;
    int kills;
    int deaths;
    boolean alive = true;
    long lastSwing;
    private Fighter lastAttacker;
    private long lastHit;

    Fighter(UUID id, String name, Team team) {
        this.id = id;
        this.name = name;
        this.team = team;
    }

    void hitBy(Fighter attacker, long now) {
        lastAttacker = attacker;
        lastHit = now;
    }

    /**
     * Who earns the kill if this fighter dies at {@code now}: whoever hit them in this life lately.
     */
    Optional<Fighter> killer(long now) {
        if (lastAttacker == null || now - lastHit >= KILL_CREDIT_MILLIS) return Optional.empty();
        return Optional.of(lastAttacker);
    }

    /** Starts a new life, which earlier hits no longer count toward. */
    void respawn() {
        alive = true;
        lastAttacker = null;
    }
}
