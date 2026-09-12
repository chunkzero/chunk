package dev.chunkzero.runtime.minestom.event;

import dev.chunkzero.runtime.SessionScope;

import org.jetbrains.annotations.NotNull;

import java.util.Objects;

/**
 * Called after scope cleanup settles, before its event node is detached. The scope is disposed and
 * cannot accept work. Also emitted when cleaning up failed session creation.
 */
public final class SessionDestroyEvent implements SessionEvent {
    private final @NotNull SessionScope session;

    public SessionDestroyEvent(@NotNull SessionScope session) {
        this.session = Objects.requireNonNull(session);
    }

    @Override
    public @NotNull SessionScope getSession() {
        return session;
    }
}
