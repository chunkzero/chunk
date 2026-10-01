package example.arena;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertSame;

import org.junit.jupiter.api.Test;

import java.util.List;
import java.util.UUID;

class RosterTest {
    @Test
    void leaversKeepTheirTeamAndTalliesAndStayInTheResult() {
        var roster = new Roster();
        var red = roster.join(UUID.randomUUID(), "red", List.of());
        var blue = roster.join(UUID.randomUUID(), "blue", List.of(red));
        red.kills = 2;

        // Red leaves; a newcomer would join red, but red's player returns as themself.
        var back = roster.join(red.id, "red", List.of(blue));
        assertSame(red, back);
        assertEquals(Team.RED, back.team);
        assertEquals(2, back.kills);

        roster.join(UUID.randomUUID(), "later", List.of(blue));
        assertEquals(3, roster.all().size());
    }
}
