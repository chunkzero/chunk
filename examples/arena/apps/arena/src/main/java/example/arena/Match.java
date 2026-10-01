package example.arena;

import java.util.EnumMap;
import java.util.Map;
import java.util.Optional;

/** The rules of one king-of-the-hill match, advanced once a second. */
final class Match {
    enum Phase {
        WAITING,
        COUNTDOWN,
        RUNNING,
        ENDED
    }

    record Rules(int targetScore, int timeLimitSeconds) {}

    /** Players per team, either in the match or standing on the hill. */
    record Headcount(int red, int blue) {
        int of(Team team) {
            return team == Team.RED ? red : blue;
        }
    }

    static final int COUNTDOWN_SECONDS = 10;

    private final Rules rules;
    private final Map<Team, Integer> scores = new EnumMap<>(Team.class);
    private Phase phase = Phase.WAITING;
    private int countdown;
    private int secondsLeft;
    // The team that scored this second, if any, and the winner once the match ends.
    private Team holder;
    private boolean contested;
    private Team winner;

    Match(Rules rules) {
        this.rules = rules;
        for (var team : Team.values()) scores.put(team, 0);
        secondsLeft = rules.timeLimitSeconds();
    }

    /**
     * The team that scores while {@code onHill} stand on the hill: the only team there. An empty or
     * contested hill scores nothing.
     */
    static Optional<Team> holder(Headcount onHill) {
        if (onHill.red() > 0 && onHill.blue() == 0) return Optional.of(Team.RED);
        if (onHill.blue() > 0 && onHill.red() == 0) return Optional.of(Team.BLUE);
        return Optional.empty();
    }

    /**
     * Advances one second. A match starts once both teams have a player, and ends early if one
     * empties.
     */
    void second(Headcount players, Headcount onHill) {
        var bothTeams = players.red() > 0 && players.blue() > 0;
        switch (phase) {
            case WAITING -> {
                if (bothTeams) {
                    phase = Phase.COUNTDOWN;
                    countdown = COUNTDOWN_SECONDS;
                }
            }
            case COUNTDOWN -> {
                if (!bothTeams) phase = Phase.WAITING;
                else if (--countdown == 0) phase = Phase.RUNNING;
            }
            case RUNNING -> play(players, onHill);
            case ENDED -> {}
        }
    }

    private void play(Headcount players, Headcount onHill) {
        holder = null;
        contested = onHill.red() > 0 && onHill.blue() > 0;
        for (var team : Team.values()) {
            if (players.of(team) == 0) {
                end(team.opponent());
                return;
            }
        }
        holder = holder(onHill).orElse(null);
        if (holder != null && scores.merge(holder, 1, Integer::sum) >= rules.targetScore()) {
            end(holder);
        } else if (--secondsLeft == 0) {
            var red = score(Team.RED);
            var blue = score(Team.BLUE);
            end(red == blue ? null : red > blue ? Team.RED : Team.BLUE);
        }
    }

    private void end(Team winner) {
        this.winner = winner;
        phase = Phase.ENDED;
    }

    Rules rules() {
        return rules;
    }

    Phase phase() {
        return phase;
    }

    int countdown() {
        return countdown;
    }

    int secondsLeft() {
        return secondsLeft;
    }

    int score(Team team) {
        return scores.get(team);
    }

    Optional<Team> holder() {
        return Optional.ofNullable(holder);
    }

    boolean contested() {
        return contested;
    }

    /** The winning team once the match has ended, or empty for a draw or an unfinished match. */
    Optional<Team> winner() {
        return Optional.ofNullable(winner);
    }
}
