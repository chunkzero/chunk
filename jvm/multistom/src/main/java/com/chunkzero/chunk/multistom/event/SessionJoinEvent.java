package com.chunkzero.chunk.multistom.event;

import com.chunkzero.chunk.multistom.SessionScope;

import net.minestom.server.entity.Player;
import net.minestom.server.event.trait.PlayerEvent;

import org.jetbrains.annotations.NotNull;

import java.util.Objects;

/** Called after the player's join hook succeeds, before arrival is confirmed to control. */
public final class SessionJoinEvent implements SessionEvent, PlayerEvent {
    private final @NotNull SessionScope session;
    private final @NotNull Player player;

    public SessionJoinEvent(@NotNull SessionScope session, @NotNull Player player) {
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
