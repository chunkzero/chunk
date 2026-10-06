package com.chunkzero.chunk.runtime.minestom.event;

import com.chunkzero.chunk.runtime.SessionScope;

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
