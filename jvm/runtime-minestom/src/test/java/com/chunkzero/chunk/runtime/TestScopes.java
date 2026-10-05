package com.chunkzero.chunk.runtime;

import net.minestom.server.ServerProcess;

import java.util.concurrent.CompletableFuture;

/** Session scopes for tests in other packages, whose tick thread is the calling thread. */
public final class TestScopes {
    private TestScopes() {}

    public static SessionScope create(ServerProcess process) {
        var ticks = new TickExecutor();
        ticks.flush();
        return new SessionScope(
                process, "test", ticks, () -> CompletableFuture.completedFuture(null), null);
    }
}
