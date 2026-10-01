package example.arena;

import net.kyori.adventure.bossbar.BossBar;
import net.kyori.adventure.sound.Sound;
import net.kyori.adventure.text.Component;
import net.kyori.adventure.text.format.NamedTextColor;
import net.kyori.adventure.title.Title;
import net.minestom.server.entity.Player;
import net.minestom.server.sound.SoundEvent;

import java.time.Duration;
import java.util.Map;
import java.util.Optional;

/** What players see of the match: one boss bar for the session, and titles and sounds. */
final class Hud {
    private final BossBar bar =
            BossBar.bossBar(Component.empty(), 1f, BossBar.Color.WHITE, BossBar.Overlay.PROGRESS);

    void show(Player player) {
        player.showBossBar(bar);
    }

    void hide(Player player) {
        player.hideBossBar(bar);
    }

    void update(Match match) {
        switch (match.phase()) {
            case WAITING ->
                    set(Component.text("Waiting for a challenger"), 1f, BossBar.Color.WHITE);
            case COUNTDOWN ->
                    set(
                            Component.text("The match starts in " + match.countdown()),
                            (float) match.countdown() / Match.COUNTDOWN_SECONDS,
                            BossBar.Color.YELLOW);
            case RUNNING ->
                    set(
                            scores(match)
                                    .append(Component.text("  |  "))
                                    .append(hill(match))
                                    .append(Component.text("  |  " + clock(match.secondsLeft()))),
                            (float) match.secondsLeft() / match.rules().timeLimitSeconds(),
                            match.holder()
                                    .map(
                                            team ->
                                                    team == Team.RED
                                                            ? BossBar.Color.RED
                                                            : BossBar.Color.BLUE)
                                    .orElse(BossBar.Color.WHITE));
            case ENDED ->
                    set(
                            outcome(match.winner())
                                    .append(Component.text("  "))
                                    .append(scores(match)),
                            1f,
                            BossBar.Color.PURPLE);
        }
    }

    void countdown(Iterable<Player> players, int seconds) {
        for (var player : players) {
            player.showTitle(
                    title(Component.text(seconds, NamedTextColor.YELLOW), Component.empty()));
            player.playSound(sound(SoundEvent.BLOCK_NOTE_BLOCK_PLING, 1f));
        }
    }

    void start(Iterable<Player> players) {
        for (var player : players) {
            player.showTitle(
                    title(
                            Component.text("Fight!", NamedTextColor.GOLD),
                            Component.text("Hold the hill in the centre to score")));
            player.playSound(sound(SoundEvent.EVENT_RAID_HORN, 1f));
        }
    }

    void hillChanged(Iterable<Player> players, Match match) {
        var message =
                match.holder()
                        .map(team -> team.label().append(Component.text(" holds the hill")))
                        .orElse(hill(match));
        for (var player : players) {
            player.sendActionBar(message);
            if (match.holder().isPresent()) player.playSound(sound(SoundEvent.BLOCK_BELL_USE, 1f));
        }
    }

    void result(Map<Player, Fighter> fighters, Match match) {
        var winner = match.winner();
        fighters.forEach(
                (player, fighter) -> {
                    var won = winner.isPresent() && winner.get() == fighter.team;
                    var lost = winner.isPresent() && !won;
                    player.showTitle(
                            title(
                                    won
                                            ? Component.text("Victory!", NamedTextColor.GOLD)
                                            : lost
                                                    ? Component.text("Defeat", NamedTextColor.RED)
                                                    : Component.text("Draw", NamedTextColor.YELLOW),
                                    scores(match)));
                    player.playSound(
                            won
                                    ? sound(SoundEvent.UI_TOAST_CHALLENGE_COMPLETE, 1f)
                                    : sound(SoundEvent.ENTITY_VILLAGER_NO, 1f));
                    player.sendMessage(
                            Component.text(
                                    "Your match: %d captures, %d kills, %d deaths. Back to the keep shortly."
                                            .formatted(
                                                    fighter.captures,
                                                    fighter.kills,
                                                    fighter.deaths),
                                    NamedTextColor.GRAY));
                });
    }

    void died(Player player, Component cause) {
        player.showTitle(title(Component.text("You died", NamedTextColor.RED), cause));
        player.playSound(sound(SoundEvent.ENTITY_PLAYER_HURT, 0.8f));
    }

    void scored(Player player) {
        player.playSound(sound(SoundEvent.ENTITY_EXPERIENCE_ORB_PICKUP, 1f));
    }

    static String clock(int seconds) {
        return "%d:%02d".formatted(seconds / 60, seconds % 60);
    }

    private void set(Component name, float progress, BossBar.Color color) {
        bar.name(name).progress(Math.clamp(progress, 0f, 1f)).color(color);
    }

    private static Component scores(Match match) {
        return Team.RED
                .label()
                .append(
                        Component.text(
                                " " + match.score(Team.RED) + " - " + match.score(Team.BLUE) + " "))
                .append(Team.BLUE.label());
    }

    private static Component hill(Match match) {
        if (match.contested()) return Component.text("Hill contested", NamedTextColor.LIGHT_PURPLE);
        return match.holder()
                .map(team -> Component.text("Hill: ").append(team.label()))
                .orElse(Component.text("Hill empty", NamedTextColor.GRAY));
    }

    private static Component outcome(Optional<Team> winner) {
        return winner.map(team -> team.label().append(Component.text(" wins!")))
                .orElse(Component.text("Draw!", NamedTextColor.YELLOW));
    }

    private static Title title(Component title, Component subtitle) {
        return Title.title(
                title,
                subtitle,
                Title.Times.times(
                        Duration.ofMillis(150), Duration.ofSeconds(2), Duration.ofMillis(400)));
    }

    private static Sound sound(SoundEvent event, float pitch) {
        return Sound.sound(event, Sound.Source.MASTER, 1f, pitch);
    }
}
