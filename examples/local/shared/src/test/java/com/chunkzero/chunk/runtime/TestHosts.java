package com.chunkzero.chunk.runtime;

import chunk.sync.v1.Jvm.JvmDelivery;
import chunk.sync.v1.Jvm.JvmSession;
import chunk.sync.v1.Jvm.JvmSessionStatus;

import com.chunkzero.chunk.backend.client.BackendSession;

import java.util.concurrent.CompletableFuture;
import java.util.function.Function;

/** Test bridge to the package-private parts of {@link ChunkSessions} and {@link Delivery}. */
public final class TestHosts {
    private TestHosts() {}

    public static ChunkSessions detached(
            SessionHandler handler, Function<String, BackendSession> backends) {
        return ChunkSessions.detached(handler, backends);
    }

    public static CompletableFuture<JvmSessionStatus> create(
            ChunkSessions host, String id, JvmSession session) {
        return host.create(id, session);
    }

    public static CompletableFuture<JvmSessionStatus> finish(
            ChunkSessions host, String id, JvmSession session) {
        return host.finish(id, session);
    }

    public static Delivery delivery(String id, JvmDelivery spec) {
        return new Delivery(null, id, spec, 0);
    }
}
