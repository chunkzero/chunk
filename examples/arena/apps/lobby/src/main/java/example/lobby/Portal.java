package example.lobby;

import dev.chunkzero.generated.Destinations;
import dev.chunkzero.runtime.MoveResult;
import dev.chunkzero.runtime.SessionScope;

import net.kyori.adventure.sound.Sound;
import net.kyori.adventure.text.Component;
import net.kyori.adventure.text.format.NamedTextColor;
import net.minestom.server.coordinate.Point;
import net.minestom.server.entity.Player;
import net.minestom.server.event.player.PlayerMoveEvent;
import net.minestom.server.sound.SoundEvent;

import java.util.HashSet;
import java.util.Set;

/** Queues players for the arena when they walk into the gate south of spawn. */
final class Portal {
    private final SessionScope scope;
    private final Set<Player> queueing = new HashSet<>();

    Portal(SessionScope scope) {
        this.scope = scope;
    }

    void onMove(PlayerMoveEvent event) {
        var player = event.getPlayer();
        if (!inGate(event.getNewPosition()) || inGate(player.getPosition())) return;
        if (!queueing.add(player)) return;
        player.sendActionBar(Component.text("Joining the arena...", NamedTextColor.GOLD));
        player.playSound(
                Sound.sound(SoundEvent.BLOCK_PORTAL_TRIGGER, Sound.Source.MASTER, 0.4f, 1.5f));
        scope.move(player, Destinations.Arena.koth)
                .whenComplete(
                        (result, error) ->
                                scope.onTick(
                                        () -> {
                                            queueing.remove(player);
                                            if (player.isOnline()) report(player, result, error);
                                        }));
    }

    private static boolean inGate(Point position) {
        return position.x() >= -1
                && position.x() < 2
                && position.y() >= 65
                && position.y() < 69
                && position.z() >= 12
                && position.z() < 14;
    }

    private static void report(Player player, MoveResult result, Throwable error) {
        if (error != null) {
            refuse(player, "The arena can't be reached right now. Walk through the gate again.");
            return;
        }
        switch (result) {
            case ACCEPTED, OFFLINE -> {}
            case FULL -> refuse(player, "The arena is full. Try again in a moment.");
            case STALE -> refuse(player, "You're already on your way. Hold on.");
            case UNKNOWN_DESTINATION -> refuse(player, "This release has no arena.");
        }
    }

    private static void refuse(Player player, String message) {
        player.sendMessage(Component.text(message, NamedTextColor.RED));
        player.playSound(Sound.sound(SoundEvent.ENTITY_VILLAGER_NO, Sound.Source.MASTER, 1f, 1f));
    }
}
