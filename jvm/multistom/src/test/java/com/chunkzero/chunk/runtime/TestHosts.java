package com.chunkzero.chunk.runtime;

import chunk.sync.v1.CoreOuterClass.Position;
import chunk.sync.v1.Jvm.JvmMethodResult;
import chunk.sync.v1.Jvm.JvmReport;
import chunk.sync.v1.Jvm.JvmSession;
import chunk.sync.v1.Jvm.JvmSessionPhase;
import chunk.sync.v1.Jvm.JvmSessionStatus;

import com.chunkzero.chunk.backend.api.Destination;
import com.chunkzero.chunk.backend.client.BackendSession;
import com.chunkzero.chunk.multistom.FakeCore;
import com.chunkzero.chunk.runtime.control.ProcessState;
import com.google.protobuf.ByteString;

import org.jetbrains.annotations.Nullable;

import java.time.Duration;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionStage;
import java.util.function.BooleanSupplier;
import java.util.function.LongSupplier;

/**
 * Test bridge to the package-private parts of {@link ChunkSessions}. Its hosts run handler
 * callbacks on the calling thread.
 */
public final class TestHosts {
    private TestHosts() {}

    public static ChunkSessions detached(SessionHandler handler) {
        return ChunkSessions.detached(handler, session -> null);
    }

    public static ChunkSessions withDeadline(
            SessionHandler handler, Duration deadline, LongSupplier nanos) {
        return new ChunkSessions(handler, link(null, nanos), Runnable::run, deadline, null);
    }

    /** Reports to {@code core}, and expires deliveries by {@code nanos}. */
    public static ChunkSessions linked(SessionHandler handler, FakeCore core, LongSupplier nanos) {
        return new ChunkSessions(
                handler, link(core, nanos), Runnable::run, ChunkSessions.CREATE_DEADLINE, null);
    }

    public static void sweep(ChunkSessions host) {
        host.sweep();
    }

    public static void apply(ChunkSessions host, Map<String, ByteString> entries) {
        host.apply(entries);
    }

    public static JvmReport inventory(ChunkSessions host) {
        return host.inventory();
    }

    public static ProcessState state(ChunkSessions host) {
        return host.state();
    }

    public static void forget(ChunkSessions host) {
        host.forget();
    }

    public static CompletableFuture<JvmSessionStatus> create(
            ChunkSessions host, String id, JvmSession session) {
        return host.create(id, session);
    }

    public static CompletableFuture<JvmSessionStatus> finish(
            ChunkSessions host, String id, JvmSession session) {
        return host.finish(id, session);
    }

    public static @Nullable JvmSessionPhase phase(ChunkSessions host, String id) {
        return host.phase(id);
    }

    public static boolean terminal(JvmSessionPhase phase) {
        return ChunkSessions.terminal(phase);
    }

    public static SessionMethod method(
            String name, String argumentsJson, Delivery delivery, BooleanSupplier start) {
        return new SessionMethod(name, argumentsJson, delivery, start);
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
