package com.chunkzero.chunk.multistom.event;

import com.chunkzero.chunk.multistom.SessionScope;

import net.minestom.server.entity.Player;
import net.minestom.server.event.trait.PlayerEvent;

import org.jetbrains.annotations.NotNull;

import java.util.Objects;

/**
 * Called after a joined player is removed and its leave hook settles, including cleanup failures.
 * Player-owned resources have been released; the player's session APIs are no longer available.
 */
public final class SessionLeaveEvent implements SessionEvent, PlayerEvent {
    private final @NotNull SessionScope session;
    private final @NotNull Player player;

    public SessionLeaveEvent(@NotNull SessionScope session, @NotNull Player player) {
        this.session = Objects.requireNonNull(session);
        this.player = Objects.requireNonNull(player);
    }

    @Override
    public @NotNull SessionScope getSession() {
        return session;
    }

    @Override
    public @NotNull Player getPlayer() {
        return player;
    }
}
