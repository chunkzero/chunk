package com.chunkzero.chunk.multistom;

import org.jetbrains.annotations.ApiStatus;
import org.jetbrains.annotations.Nullable;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ConcurrentLinkedQueue;
import java.util.function.Supplier;

@ApiStatus.Internal
public final class TickExecutor {
    private final ConcurrentLinkedQueue<Runnable> pending = new ConcurrentLinkedQueue<>();
    private @Nullable Thread thread;

    TickExecutor() {}

    boolean isCurrentThread() {
        return Thread.currentThread() == thread;
    }

    void checkThread() {
        if (!isCurrentThread())
            throw new IllegalStateException("Use SessionScope.onTick for world changes");
    }

    public <T> CompletableFuture<T> submit(Supplier<T> action) {
        var result = new CompletableFuture<T>();
        pending.add(
                () -> {
                    try {
                        result.complete(action.get());
                    } catch (Exception error) {
                        result.completeExceptionally(error);
                    }
                });
        return result;
    }

    void flush() {
        thread = Thread.currentThread();
        var count = pending.size();
        for (var i = 0; i < count; i++) {
            var action = pending.poll();
            if (action != null) action.run();
        }
    }
}
