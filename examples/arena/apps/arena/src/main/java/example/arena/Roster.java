package example.arena;

import java.util.Collection;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.UUID;

/**
 * Everyone who has played in the current match, by player ID. Leaving doesn't remove a fighter: a
 * player who comes back keeps their team and tallies, and the result counts everyone who played.
 */
final class Roster {
    private final Map<UUID, Fighter> fighters = new LinkedHashMap<>();

    /**
     * The player's fighter from earlier in this match, or a new one on the team with fewer of the
     * players {@code here}.
     */
    Fighter join(UUID id, String name, Collection<Fighter> here) {
        return fighters.computeIfAbsent(
                id,
                ignored -> {
                    var red = here.stream().filter(fighter -> fighter.team == Team.RED).count();
                    return new Fighter(id, name, red * 2 <= here.size() ? Team.RED : Team.BLUE);
                });
    }

    /** Drops a player who left before the match began, so they don't count as having played. */
    void remove(UUID id) {
        fighters.remove(id);
    }

    List<Fighter> all() {
        return List.copyOf(fighters.values());
    }

    void clear() {
        fighters.clear();
    }
}
