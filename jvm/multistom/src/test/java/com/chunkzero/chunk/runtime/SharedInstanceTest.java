package com.chunkzero.chunk.runtime;

import static org.junit.jupiter.api.Assertions.*;

import net.minestom.server.ServerProcess;
import net.minestom.server.instance.SharedInstance;

import org.junit.jupiter.api.Test;

import java.util.List;
import java.util.UUID;
import java.util.concurrent.CompletableFuture;

class SharedInstanceTest {
    @Test
    void aSessionsSharedViewGoesAwayWithItAndLeavesTheContainer() throws Exception {
        var process = ServerProcess.create();
        var ticks = new TickExecutor();
        ticks.flush();
        try {
            var container = process.instanceManager().createInstanceContainer();
            var scope =
                    new SessionScope(
                            process,
                            "lobby",
                            ticks,
                            () -> CompletableFuture.completedFuture(null),
                            null);
            var view =
                    scope.registerSharedInstance(new SharedInstance(UUID.randomUUID(), container));
            assertEquals(List.of(view), scope.getInstances());
            assertTrue(view.isRegistered());
            scope.dispose();
            assertFalse(view.isRegistered());
            assertTrue(container.isRegistered());
            assertEquals(List.of(), container.getSharedInstances());
        } finally {
            process.stop();
        }
    }
}
