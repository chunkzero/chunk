package example.arena;

import static org.junit.jupiter.api.Assertions.assertEquals;

import org.junit.jupiter.api.Test;

import java.util.Optional;
import java.util.UUID;

class FighterTest {
    @Test
    void killCreditLastsTenSecondsAndEndsWithTheLife() {
        var victim = new Fighter(UUID.randomUUID(), "victim", Team.RED);
        var attacker = new Fighter(UUID.randomUUID(), "attacker", Team.BLUE);
        victim.hitBy(attacker, 1_000);
        assertEquals(Optional.of(attacker), victim.killer(10_999));
        assertEquals(Optional.empty(), victim.killer(11_000));

        victim.respawn();
        assertEquals(Optional.empty(), victim.killer(2_000));
    }
}
