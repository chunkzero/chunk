package example.arena;

import com.chunkzero.chunk.backend.client.OperationId;
import com.chunkzero.chunk.generated.BackendClient;
import com.chunkzero.chunk.generated.Destinations;
import com.chunkzero.chunk.generated.SessionMethods;
import com.chunkzero.chunk.generated.Worlds;
import com.chunkzero.chunk.runtime.MoveResult;
import com.chunkzero.chunk.runtime.Session;
import com.chunkzero.chunk.runtime.SessionScope;
import com.chunkzero.chunk.runtime.assets.Assets;

import net.kyori.adventure.text.Component;
import net.kyori.adventure.text.format.NamedTextColor;
import net.minestom.server.coordinate.Point;
import net.minestom.server.entity.Player;

import java.time.Duration;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionStage;

/**
 * An arena running king-of-the-hill matches: teams, the once-a-second clock, the result and the way
 * home. Once a finished match has sent everyone home, the arena waits for the next one.
 */
public final class ArenaSession extends Session implements SessionMethods.Arena.Koth.Status {
    private static final int SECONDS_BEFORE_LOBBY = 6;
    private static final int MOVE_EXPIRY_SECONDS = 10;

    private final Match.Rules rules;
    private final Roster roster = new Roster();
    // The fighters of the players here now.
    private final Map<Player, Fighter> fighters = new LinkedHashMap<>();
    private final PendingMoves<Player> leaving = new PendingMoves<>(MOVE_EXPIRY_SECONDS);
    private final Hud hud = new Hud();
    private SessionScope scope;
    private Combat combat;
    private Match match;
    private int matchesPlayed;
    private CompletableFuture<Void> recorded;
    private int secondsSinceEnd;
    private int clock;

    ArenaSession(Match.Rules rules) {
        this.rules = rules;
        match = new Match(rules);
    }

    @Override
    public CompletionStage<Void> onCreate(SessionScope scope) {
        this.scope = scope;
        return Assets.world(Worlds.Arena.ARENA)
                .copy(scope)
                .thenCompose(instance -> scope.onTick(this::open));
    }

    private void open() {
        combat = new Combat(scope, fighters, hud, () -> match.phase() == Match.Phase.RUNNING);
        scope.repeatEvery(Duration.ofSeconds(1), this::second);
    }

    @Override
    public CompletionStage<Void> onJoin(Player player) {
        var fighter = roster.join(player.getUuid(), player.getUsername(), fighters.values());
        fighters.put(player, fighter);
        hud.show(player);
        if (match.phase() == Match.Phase.ENDED) {
            player.sendMessage(
                    Component.text(
                            "This match just ended. Back to the keep shortly.",
                            NamedTextColor.GRAY));
        } else {
            player.sendMessage(
                    Component.text("You fight for ")
                            .append(fighter.team.label())
                            .append(
                                    Component.text(
                                            ". Stand alone on the hill in the centre to score."
                                                    + " First to %d wins."
                                                            .formatted(rules.targetScore()))));
        }
        return combat.spawn(player, fighter);
    }

    @Override
    public CompletionStage<Void> onLeave(Player player) {
        fighters.remove(player);
        if (match.phase() == Match.Phase.WAITING || match.phase() == Match.Phase.COUNTDOWN) {
            roster.remove(player.getUuid());
        }
        leaving.forget(player);
        hud.hide(player);
        return CompletableFuture.completedFuture(null);
    }

    @Override
    public String status(SessionMethods.Arena.Koth.Status.Args args) {
        var scores = "Red %d - %d Blue".formatted(match.score(Team.RED), match.score(Team.BLUE));
        return switch (match.phase()) {
            case WAITING -> "Waiting for a challenger (%d here)".formatted(fighters.size());
            case COUNTDOWN -> "Starting in %d seconds".formatted(match.countdown());
            case RUNNING ->
                    "%s, %s left, first to %d"
                            .formatted(scores, Hud.clock(match.secondsLeft()), rules.targetScore());
            case ENDED -> "Finished: " + scores;
        };
    }

    private void second() {
        clock++;
        var before = match.phase();
        var heldBefore = match.holder();
        var contestedBefore = match.contested();
        var onHill = new HashSet<Player>();
        fighters.forEach(
                (player, fighter) -> {
                    if (fighter.alive && onHill(player.getPosition())) onHill.add(player);
                });
        match.second(headcount(fighters.keySet()), headcount(onHill));

        var phase = match.phase();
        if (phase == Match.Phase.COUNTDOWN && match.countdown() <= 5) {
            hud.countdown(fighters.keySet(), match.countdown());
        }
        if (before == Match.Phase.COUNTDOWN && phase == Match.Phase.RUNNING) {
            fighters.forEach(combat::spawn);
            hud.start(fighters.keySet());
        }
        // A holder means only that team stands on the hill, so everyone there helped capture it.
        if (before == Match.Phase.RUNNING && match.holder().isPresent()) {
            onHill.forEach(player -> fighters.get(player).captures++);
        }
        if (phase == Match.Phase.RUNNING
                && (!match.holder().equals(heldBefore) || match.contested() != contestedBefore)) {
            hud.hillChanged(fighters.keySet(), match);
        }
        if (before != Match.Phase.ENDED && phase == Match.Phase.ENDED) {
            hud.result(fighters, match);
            var operation = new OperationId(scope.getId() + "/match-" + ++matchesPlayed);
            recorded =
                    Results.record(
                            scope.component(BackendClient.class),
                            operation,
                            roster.all(),
                            match.winner());
        }
        if (phase == Match.Phase.ENDED) sendHome();
        hud.update(match);
    }

    /**
     * Once the result is recorded and players have seen it, moves everyone back to the lobby. An
     * emptied arena starts over, ready for the next players the lobby queues.
     */
    private void sendHome() {
        if (fighters.isEmpty()) {
            roster.clear();
            match = new Match(rules);
            secondsSinceEnd = 0;
            return;
        }
        if (!recorded.isDone() || ++secondsSinceEnd < SECONDS_BEFORE_LOBBY) return;
        for (var player : fighters.keySet()) {
            if (leaving.start(player, clock)) {
                scope.move(player, Destinations.Lobby.main)
                        .whenComplete((result, error) -> scope.onTick(() -> moved(player, result)));
            }
        }
    }

    /** A refused or failed move leaves the player here, to try again next second. */
    private void moved(Player player, MoveResult result) {
        if (result != MoveResult.ACCEPTED) leaving.refused(player);
    }

    private Match.Headcount headcount(Set<Player> players) {
        var red = (int) players.stream().filter(p -> fighters.get(p).team == Team.RED).count();
        return new Match.Headcount(red, players.size() - red);
    }

    /** Within 4.5 blocks of the hill's centre (0.0, 0.5), on or above its top at y = 71. */
    static boolean onHill(Point position) {
        var dz = position.z() - 0.5;
        return position.x() * position.x() + dz * dz <= 4.5 * 4.5
                && position.y() >= 71
                && position.y() <= 76;
    }
}
