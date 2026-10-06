package com.chunkzero.chunk.runtime;

import chunk.sync.v1.CoreOuterClass.Position;
import chunk.sync.v1.Jvm.JvmMethodResult;

import com.chunkzero.chunk.backend.api.Destination;
import com.chunkzero.chunk.backend.client.BackendSession;

import java.time.Duration;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionStage;
import java.util.function.LongSupplier;

/** Session hosts for tests, whose handler callbacks run on the calling thread. */
final class TestHosts {
    private TestHosts() {}

    static ChunkSessions detached(SessionHandler handler) {
        return ChunkSessions.detached(handler, session -> null);
    }

    static ChunkSessions withDeadline(SessionHandler handler, Duration deadline) {
        return new ChunkSessions(
                handler, link(null, System::nanoTime), Runnable::run, deadline, null);
    }

    /** Reports to {@code core}, and expires deliveries by {@code nanos}. */
    static ChunkSessions linked(SessionHandler handler, FakeCore core, LongSupplier nanos) {
        return new ChunkSessions(
                handler, link(core, nanos), Runnable::run, ChunkSessions.CREATE_DEADLINE, null);
    }

    private static ChunkSessions.Link link(FakeCore core, LongSupplier nanos) {
        return new ChunkSessions.Link() {
            @Override
            public boolean acceptsWork() {
                return true;
            }

            @Override
            public BackendSession backend(String session) {
                return null;
            }

            @Override
            public CompletionStage<MoveResult> move(
                    String delivery, Position generation, Destination destination) {
                return CompletableFuture.failedFuture(
                        new IllegalStateException("Moves unavailable"));
            }

            @Override
            public void methodResult(String operation, JvmMethodResult result) {
                if (core != null) core.methodResult(operation, result);
            }

            @Override
            public void flush() {
                if (core != null) core.wake();
            }

            @Override
            public long nanoTime() {
                return nanos.getAsLong();
            }
        };
    }
}
