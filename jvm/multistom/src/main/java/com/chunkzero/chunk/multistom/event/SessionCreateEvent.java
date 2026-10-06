package com.chunkzero.chunk.multistom.event;

import com.chunkzero.chunk.multistom.SessionScope;

import org.jetbrains.annotations.NotNull;

import java.util.Objects;

/** Called after session creation succeeds and its instances are available. */
public final class SessionCreateEvent implements SessionEvent {
    private final @NotNull SessionScope session;

    public SessionCreateEvent(@NotNull SessionScope session) {
        this.session = Objects.requireNonNull(session);
    }

    @Override
    public @NotNull SessionScope getSession() {
        return session;
    }
}
