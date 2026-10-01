package example.arena;

import static org.junit.jupiter.api.Assertions.assertEquals;

import example.arena.Match.Headcount;
import example.arena.Match.Phase;

import org.junit.jupiter.api.Test;

import java.util.Optional;

class MatchTest {
    private static final Headcount ONE_EACH = new Headcount(1, 1);
    private static final Headcount EMPTY = new Headcount(0, 0);

    @Test
    void onlyATeamAloneOnTheHillScores() {
        var match = running(new Match.Rules(10, 60));
        match.second(ONE_EACH, new Headcount(1, 0));
        match.second(ONE_EACH, new Headcount(1, 1));
        match.second(ONE_EACH, EMPTY);
        match.second(ONE_EACH, new Headcount(0, 1));
        assertEquals(1, match.score(Team.RED));
        assertEquals(1, match.score(Team.BLUE));
    }

    @Test
    void theFirstTeamToTheTargetWins() {
        var match = running(new Match.Rules(3, 60));
        for (var second = 0; second < 3; second++) match.second(ONE_EACH, new Headcount(0, 2));
        assertEquals(Phase.ENDED, match.phase());
        assertEquals(Optional.of(Team.BLUE), match.winner());
    }

    @Test
    void theHigherScoreWinsWhenTimeRunsOutAndEqualScoresDraw() {
        var won = running(new Match.Rules(10, 3));
        won.second(ONE_EACH, new Headcount(1, 0));
        won.second(ONE_EACH, EMPTY);
        won.second(ONE_EACH, EMPTY);
        assertEquals(Optional.of(Team.RED), won.winner());

        var drawn = running(new Match.Rules(10, 2));
        drawn.second(ONE_EACH, EMPTY);
        drawn.second(ONE_EACH, EMPTY);
        assertEquals(Phase.ENDED, drawn.phase());
        assertEquals(Optional.empty(), drawn.winner());
    }

    @Test
    void aMatchWaitsForBothTeamsAndCountsDownBeforeItStarts() {
        var match = new Match(new Match.Rules(10, 60));
        match.second(new Headcount(2, 0), EMPTY);
        assertEquals(Phase.WAITING, match.phase());
        match.second(ONE_EACH, EMPTY);
        assertEquals(Phase.COUNTDOWN, match.phase());
        match.second(new Headcount(1, 0), EMPTY);
        assertEquals(Phase.WAITING, match.phase());
    }

    private static Match running(Match.Rules rules) {
        var match = new Match(rules);
        for (var second = 0; second <= Match.COUNTDOWN_SECONDS; second++)
            match.second(ONE_EACH, EMPTY);
        assertEquals(Phase.RUNNING, match.phase());
        return match;
    }
}
