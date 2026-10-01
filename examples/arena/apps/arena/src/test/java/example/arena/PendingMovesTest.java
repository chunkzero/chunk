package example.arena;

import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertTrue;

import org.junit.jupiter.api.Test;

class PendingMovesTest {
    @Test
    void anAcceptedMoveThatNeverLandsIsTriedAgainOnceItExpires() {
        var moves = new PendingMoves<String>(10);
        assertTrue(moves.start("player", 1));
        assertFalse(moves.start("player", 10));
        assertTrue(moves.start("player", 11));

        moves.refused("player");
        assertTrue(moves.start("player", 12));
    }
}
