package example.arena;

import dev.chunkzero.backend.api.PlayerId;
import dev.chunkzero.backend.client.OperationId;
import dev.chunkzero.generated.BackendClient;
import dev.chunkzero.generated.BackendTypes.Shared.Stats.RecordMatchArgs;

import java.util.List;
import java.util.Locale;
import java.util.Optional;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;

/** Sends a finished match to the backend's {@code recordMatch} mutation. */
final class Results {
    private static final System.Logger LOG = System.getLogger(Results.class.getName());
    private static final int ATTEMPTS = 3;

    private Results() {}

    /**
     * Records the match, retrying under the same operation ID so a retry after a lost reply can't
     * count it twice. Completes either way; a result that can't be recorded is logged.
     */
    static CompletableFuture<Void> record(
            BackendClient backend,
            OperationId operation,
            List<Fighter> fighters,
            Optional<Team> winner) {
        var players = fighters.stream().map(Results::player).toList();
        var args =
                new RecordMatchArgs(
                        players,
                        winner.map(team -> RecordMatchArgs.Winner.valueOf(wire(team)))
                                .orElse(null));
        return attempt(backend, operation, args, 1);
    }

    private static CompletableFuture<Void> attempt(
            BackendClient backend, OperationId operation, RecordMatchArgs args, int attempt) {
        return backend.shared()
                .stats()
                .recordMatch(args, operation)
                .exceptionallyCompose(
                        error -> {
                            if (attempt == ATTEMPTS) {
                                LOG.log(System.Logger.Level.WARNING, "Match not recorded", error);
                                return CompletableFuture.completedFuture(null);
                            }
                            var later = CompletableFuture.delayedExecutor(2, TimeUnit.SECONDS);
                            return CompletableFuture.runAsync(() -> {}, later)
                                    .thenCompose(
                                            ignored ->
                                                    attempt(backend, operation, args, attempt + 1));
                        });
    }

    private static RecordMatchArgs.PlayersItem player(Fighter fighter) {
        return new RecordMatchArgs.PlayersItem(
                (long) fighter.captures,
                (long) fighter.deaths,
                (long) fighter.kills,
                fighter.name,
                new PlayerId(fighter.id.toString()),
                RecordMatchArgs.PlayersItem.Team.valueOf(wire(fighter.team)));
    }

    private static String wire(Team team) {
        return team.name().toLowerCase(Locale.ROOT);
    }
}
