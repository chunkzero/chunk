package com.chunkzero.chunk.runtime;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionStage;

/**
 * Decides what a session is and how it is isolated, for {@link ChunkSessions}. Callbacks run one at
 * a time on the host's own thread and must return promptly; report outcomes through the {@link
 * SessionControl}, from any thread.
 */
public interface SessionHandler {
    /**
     * Builds the session. Call {@link SessionControl#ready()} once it admits players, or {@link
     * SessionControl#fail(Throwable)}. A session not ready within 10 seconds fails. Throwing fails
     * the session.
     */
    void create(SessionControl session);

    /**
     * Tears the session down once its players have been released, and then calls {@link
     * SessionControl#ended()}. Runs after a failed creation too. Throwing ends the session failed.
     * Until it ends, the session keeps counting against this JVM's capacity.
     */
    void finish(SessionControl session);

    /**
     * Runs session method {@code method} of a ready session for one of its arrived players, and
     * completes with its result as JSON, in at most 48 KiB. Call {@link SessionMethod#start()}
     * right before running the method's effects, and do not run them if it returns false. A call
     * that completes without having started is cancelled. By default, sessions declare no methods.
     */
    default CompletionStage<String> method(SessionControl session, SessionMethod call) {
        return CompletableFuture.failedFuture(
                new IllegalArgumentException("Undeclared session method: " + call.name()));
    }
}
