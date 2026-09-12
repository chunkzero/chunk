package dev.chunkzero.runtime.minestom.event;

import dev.chunkzero.runtime.SessionScope;

import net.minestom.server.event.Event;

import org.jetbrains.annotations.NotNull;

/**
 * Session lifecycle notification dispatched synchronously on the process tick thread. Listeners
 * observe lifecycle transitions; asynchronous setup and cleanup belong in the session hooks.
 */
public interface SessionEvent extends Event {
    /** The owning scope, including its session ID and generation. */
    @NotNull
    SessionScope getSession();
}
