package com.chunkzero.chunk.runtime;

import chunk.sync.v1.Jvm.JvmSession;
import chunk.sync.v1.Jvm.JvmSessionPhase;
import chunk.sync.v1.Jvm.JvmSessionStatus;

import com.chunkzero.chunk.backend.client.BackendSession;

import org.jetbrains.annotations.Nullable;

import tools.jackson.databind.JsonNode;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionStage;

/**
 * One session core asked this JVM to run, and how its {@link SessionHandler} reports on it. Its
 * methods are thread-safe.
 */
public final class SessionControl {
    private final ChunkSessions owner;
    private final String id;
    private final JvmSession spec;
    private final JsonNode configuration;
    private final @Nullable BackendSession backend;
    final long startedAt;

    // Guarded by owner.
    JvmSessionPhase phase = JvmSessionPhase.JVM_SESSION_PHASE_STARTING;
    boolean settled;
    boolean finishing;
    boolean handlerFinishing;
    @Nullable Throwable failure;

    final CompletableFuture<JvmSessionStatus> created = new CompletableFuture<>();
    final CompletableFuture<Void> ended = new CompletableFuture<>();

    SessionControl(
            ChunkSessions owner,
            String id,
            JvmSession spec,
            JsonNode configuration,
            @Nullable BackendSession backend,
            long startedAt) {
        this.owner = owner;
        this.id = id;
        this.spec = spec;
        this.configuration = configuration;
        this.backend = backend;
        this.startedAt = startedAt;
    }

    /** The session's ID, unique within the deployment. */
    public String id() {
        return id;
    }

    /** The session type, as {@code <app>/<implementation>}. */
    public String type() {
        return spec.getSessionType();
    }

    /** How many players the session admits at once, from 1 to 128. */
    public int capacity() {
        return spec.getCapacity();
    }

    /** The session's creation configuration, a JSON object. */
    public String configurationJson() {
        return configuration.toString();
    }

    /** Calls the backend as this session; closed once the session has ended. */
    public @Nullable BackendSession backend() {
        return backend;
    }

    /**
     * The session admits players. Ignored unless it is still starting.
     *
     * @return whether this call made the session ready
     */
    public boolean ready() {
        return owner.ready(this);
    }

    /** The session can't continue: it ends, and is reported failed once its handler finished it. */
    public void fail(Throwable error) {
        owner.fail(this, error);
    }

    /**
     * Ends the session: its players are released and then its handler finishes it. Completes once
     * it ended, or exceptionally if it failed.
     */
    public CompletionStage<Void> finish() {
        return owner.finish(this);
    }

    /** The handler finished the session. */
    public void ended() {
        owner.ended(this, null);
    }

    /** The handler finished the session, which failed with {@code error}. */
    public void ended(Throwable error) {
        owner.ended(this, error);
    }

    /**
     * Whether the session is ready: {@link #ready()} took effect and it has not started finishing.
     */
    public boolean isReady() {
        synchronized (owner) {
            return phase == JvmSessionPhase.JVM_SESSION_PHASE_READY;
        }
    }

    JsonNode configuration() {
        return configuration;
    }

    JvmSession spec() {
        return spec;
    }

    JvmSessionStatus status(int prepared, int attached) {
        synchronized (owner) {
            return JvmSessionStatus.newBuilder()
                    .setId(id)
                    .setSessionType(spec.getSessionType())
                    .setCapacity(spec.getCapacity())
                    .setPhase(phase)
                    .setPrepared(prepared)
                    .setAttached(attached)
                    .build();
        }
    }
}
