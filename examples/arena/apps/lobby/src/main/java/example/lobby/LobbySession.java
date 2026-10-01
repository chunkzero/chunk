package example.lobby;

import dev.chunkzero.backend.api.PlayerId;
import dev.chunkzero.generated.BackendClient;
import dev.chunkzero.generated.BackendTypes.Shared.Stats.MineResult;
import dev.chunkzero.runtime.Session;
import dev.chunkzero.runtime.SessionScope;

import example.world.PolarWorlds;

import net.hollowcube.polar.PolarWorld;
import net.kyori.adventure.text.Component;
import net.kyori.adventure.text.format.NamedTextColor;
import net.minestom.server.coordinate.Pos;
import net.minestom.server.entity.GameMode;
import net.minestom.server.entity.Player;
import net.minestom.server.event.player.PlayerMoveEvent;
import net.minestom.server.instance.Instance;

import java.util.Objects;
import java.util.concurrent.CompletionStage;

final class LobbySession extends Session {
    static final Pos SPAWN = new Pos(0.5, 65, 0.5, 180, 0);

    private SessionScope scope;

    @Override
    public CompletionStage<Void> onCreate(SessionScope scope) {
        this.scope = scope;
        return PolarWorlds.load(scope, scope.component(PolarWorld.class))
                .thenCompose(instance -> scope.onTick(() -> open(instance)));
    }

    private void open(Instance instance) {
        Leaderboard.show(scope, instance);
        scope.getEvents()
                .addListener(PlayerMoveEvent.class, new Portal(scope)::onMove)
                .addListener(PlayerMoveEvent.class, this::catchFalls);
    }

    @Override
    public CompletionStage<Void> onJoin(Player player) {
        player.setGameMode(GameMode.ADVENTURE);
        var backend =
                new BackendClient(
                        scope.own(
                                player,
                                Objects.requireNonNull(scope.getBackend())
                                        .forPlayer(new PlayerId(player.getUuid().toString()))));
        backend.shared()
                .stats()
                .mine()
                .thenAccept(stats -> scope.onTick(() -> greet(player, stats)));
        return player.teleport(SPAWN);
    }

    private void catchFalls(PlayerMoveEvent event) {
        if (event.getNewPosition().y() < 40) event.setNewPosition(SPAWN);
    }

    private static void greet(Player player, MineResult stats) {
        if (!player.isOnline()) return;
        var record =
                stats.matches() == 0
                        ? "Your first battle awaits."
                        : "Your record: %d wins in %d matches, %d captures, %d kills."
                                .formatted(
                                        stats.wins(),
                                        stats.matches(),
                                        stats.captures(),
                                        stats.kills());
        player.sendMessage(Component.text("Welcome to the keep! " + record, NamedTextColor.GOLD));
        player.sendMessage(
                Component.text(
                        "Walk through the gate to the south to fight for the hill.",
                        NamedTextColor.YELLOW));
    }
}
