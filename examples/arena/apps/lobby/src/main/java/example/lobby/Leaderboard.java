package example.lobby;

import dev.chunkzero.backend.client.QueryResult;
import dev.chunkzero.backend.client.WatchState;
import dev.chunkzero.generated.BackendClient;
import dev.chunkzero.generated.BackendTypes.Shared.Stats.LeaderboardResultItem;
import dev.chunkzero.runtime.SessionScope;

import net.kyori.adventure.text.Component;
import net.kyori.adventure.text.TextComponent;
import net.kyori.adventure.text.format.NamedTextColor;
import net.kyori.adventure.text.format.TextDecoration;
import net.minestom.server.coordinate.Pos;
import net.minestom.server.entity.Entity;
import net.minestom.server.entity.EntityType;
import net.minestom.server.entity.metadata.display.TextDisplayMeta;
import net.minestom.server.instance.Instance;

import java.util.List;

/**
 * The top-10 board on the notice board north of spawn. One watch of the leaderboard query serves
 * the whole session, however many players are in it, and redraws the board whenever a match is
 * recorded anywhere in the environment.
 */
final class Leaderboard {
    private static final int ROWS = 10;
    // Text grows upward from the entity; twelve lines at this scale centre on y = 67.5. It sits
    // just in front of the notice board, whose face is at z = -10, and faces south toward spawn.
    private static final Pos POSITION = new Pos(0.5, 66.0, -9.95, 0, 0);

    private Leaderboard() {}

    static void show(SessionScope scope, Instance instance) {
        var board = new Entity(scope.getProcess(), EntityType.TEXT_DISPLAY);
        board.editEntityMeta(
                TextDisplayMeta.class,
                meta -> {
                    meta.setLineWidth(240);
                    meta.setText(render(List.of(), false));
                });
        board.setInstance(instance, POSITION);
        var backend = scope.component(BackendClient.class);
        scope.own(
                backend.shared()
                        .stats()
                        .watchLeaderboard(state -> scope.onTick(() -> update(board, state))));
    }

    private static void update(Entity board, WatchState<List<LeaderboardResultItem>> state) {
        var snapshot = state.snapshot().orElse(null);
        if (snapshot == null || !(snapshot.result() instanceof QueryResult.Value<?>)) return;
        var rows = snapshot.result().valueOrThrow();
        board.editEntityMeta(
                TextDisplayMeta.class, meta -> meta.setText(render(rows, state.stale())));
    }

    private static Component render(List<LeaderboardResultItem> rows, boolean stale) {
        var text =
                Component.text()
                        .append(
                                Component.text(
                                        "Champions of the Hill",
                                        NamedTextColor.GOLD,
                                        TextDecoration.BOLD))
                        .append(Component.newline());
        for (var rank = 1; rank <= ROWS; rank++) {
            text.append(Component.newline())
                    .append(row(rank, rank <= rows.size() ? rows.get(rank - 1) : null));
        }
        if (stale)
            text.append(Component.newline())
                    .append(Component.text("reconnecting...", NamedTextColor.GRAY));
        return text.build();
    }

    private static TextComponent row(int rank, LeaderboardResultItem fighter) {
        var place =
                Component.text(
                        rank + ". ", rank <= 3 ? NamedTextColor.YELLOW : NamedTextColor.GRAY);
        if (fighter == null) return place.append(Component.text("---", NamedTextColor.DARK_GRAY));
        return place.append(Component.text(fighter.name(), NamedTextColor.WHITE))
                .append(
                        Component.text(
                                "  %d wins, %d captures"
                                        .formatted(fighter.wins(), fighter.captures()),
                                NamedTextColor.GRAY));
    }
}
