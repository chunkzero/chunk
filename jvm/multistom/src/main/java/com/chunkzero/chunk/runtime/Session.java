package com.chunkzero.chunk.runtime;

import net.minestom.server.entity.Player;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionStage;

/**
 * Hooks run on the process tick thread; asynchronous continuations use {@link SessionScope#onTick}.
 */
public abstract class Session {
    public CompletionStage<Void> onCreate(SessionScope scope) {
        return CompletableFuture.completedFuture(null);
    }

    public CompletionStage<Void> onJoin(Player player) {
        return CompletableFuture.completedFuture(null);
    }

    public CompletionStage<Void> onLeave(Player player) {
        return CompletableFuture.completedFuture(null);
    }

    public CompletionStage<Void> onFinish() {
        return CompletableFuture.completedFuture(null);
    }
}
