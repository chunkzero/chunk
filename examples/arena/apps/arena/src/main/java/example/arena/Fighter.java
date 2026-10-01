package example.arena;

import net.minestom.server.entity.Player;

/** One player's side and tallies in the current match. */
final class Fighter {
    final Team team;
    int captures;
    int kills;
    int deaths;
    boolean alive = true;
    long lastSwing;
    // Who last hit this fighter, and when, to credit a knock into the void.
    Player lastAttacker;
    long lastHit;

    Fighter(Team team) {
        this.team = team;
    }
}
