package com.chunkzero.chunk.runtime;

import chunk.sync.v1.CoreOuterClass.Position;
import chunk.sync.v1.Gateway.SessionDemand;
import chunk.sync.v1.Jvm.JvmMethodResult;
import chunk.sync.v1.Jvm.JvmMove;
import chunk.sync.v1.Jvm.JvmRegistration;

import com.chunkzero.chunk.backend.api.Destination;
import com.chunkzero.chunk.backend.client.BackendSession;
import com.chunkzero.chunk.runtime.bootstrap.RuntimeEnvironment;
import com.chunkzero.chunk.runtime.bootstrap.SessionBackend;
import com.chunkzero.chunk.runtime.control.CoreLink;
import com.chunkzero.chunk.runtime.control.ProcessState;

import org.jetbrains.annotations.Nullable;

import java.net.Inet6Address;
import java.net.InetAddress;
import java.util.UUID;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionStage;
import java.util.concurrent.atomic.AtomicLong;

/** Connects one immutable app process to core. */
public final class ChunkProcess implements AutoCloseable {
    private static final System.Logger LOG = System.getLogger(ChunkProcess.class.getName());
    private final RuntimeEnvironment environment;
    private final SessionBackend backend;
    private final AtomicLong ticks = new AtomicLong();
    private final ProcessHealth health = new ProcessHealth(ticks);
    private final CompletableFuture<Void> shutdown = new CompletableFuture<>();
    private volatile @Nullable CoreLink link;
    private volatile @Nullable ChunkSessions sessions;
    private @Nullable ProcessState state;
    private String playerEndpoint = "";
    private int protocol;
    private boolean closed;

    ChunkProcess(RuntimeEnvironment environment) {
        this.environment = environment;
        if (!environment.appId().matches("[A-Za-z_][A-Za-z0-9_]{0,127}")
                || environment.processGeneration() < 1)
            throw new IllegalArgumentException("Invalid app or process identity");
        if (environment.processToken().length() < 32)
            throw new IllegalArgumentException("Invalid process credential");
        backend = SessionBackend.fromEnvironment(environment);
    }

    public static ChunkProcess connect() {
        return new ChunkProcess(RuntimeEnvironment.load());
    }

    public CompletionStage<Void> shutdownRequested() {
        return shutdown.minimalCompletionStage();
    }

    /**
     * Waits until shutdown is requested.
     *
     * @throws IllegalStateException if core refused the JVM for good
     */
    public void awaitShutdown() throws InterruptedException {
        try {
            shutdown.get();
        } catch (java.util.concurrent.ExecutionException error) {
            throw new IllegalStateException(error.getCause());
        }
    }

    String app() {
        return environment.appId();
    }

    BackendSession backend(String session) {
        return backend.client(session, environment.appId());
    }

    /**
     * Asks core to move the player of {@code delivery}, at {@code generation}, to a destination.
     */
    CompletionStage<MoveResult> move(
            String delivery, Position generation, Destination destination) {
        var move =
                JvmMove.newBuilder()
                        .setDelivery(delivery)
                        .setGeneration(generation)
                        .setDestination(
                                SessionDemand.newBuilder()
                                        .setKey(destination.key())
                                        .setSessionType(destination.sessionType())
                                        .setMachineProfile(destination.machineProfile()))
                        .build();
        return backend.move(UUID.randomUUID().toString(), move)
                .thenApply(
                        result ->
                                switch (result.getRefusal()) {
                                    case MOVE_REFUSAL_UNSPECIFIED -> MoveResult.ACCEPTED;
                                    case MOVE_REFUSAL_OFFLINE -> MoveResult.OFFLINE;
                                    case MOVE_REFUSAL_STALE -> MoveResult.STALE;
                                    case MOVE_REFUSAL_FULL -> MoveResult.FULL;
                                    case MOVE_REFUSAL_UNKNOWN_DESTINATION ->
                                            MoveResult.UNKNOWN_DESTINATION;
                                    case UNRECOGNIZED ->
                                            throw new IllegalStateException("Unknown move refusal");
                                });
    }

    void progress(int sessions, int players) {
        health.tick(sessions, players);
    }

    /** Records one engine tick; core watches tick progress to tell a stalled JVM. */
    public void tick() {
        var current = sessions;
        if (current == null) progress(0, 0);
        else progress(current.activeCount(), current.players());
    }

    /**
     * Runs the sessions, deliveries and session methods core assigns this JVM through {@code
     * handler}. Call it once, before {@link #bind(int, int)}.
     */
    public synchronized ChunkSessions host(SessionHandler handler) {
        if (closed || sessions != null)
            throw new IllegalStateException("Sessions already hosted or process closed");
        var hosted =
                ChunkSessions.linked(
                        handler,
                        new ChunkSessions.Link() {
                            @Override
                            public boolean acceptsWork() {
                                return isReady();
                            }

                            @Override
                            public BackendSession backend(String session) {
                                return ChunkProcess.this.backend(session);
                            }

                            @Override
                            public CompletionStage<MoveResult> move(
                                    String delivery, Position generation, Destination destination) {
                                return ChunkProcess.this.move(delivery, generation, destination);
                            }

                            @Override
                            public void methodResult(String operation, JvmMethodResult result) {
                                ChunkProcess.this.methodResult(operation, result);
                            }

                            @Override
                            public void flush() {
                                ChunkProcess.this.flush();
                            }
                        });
        sessions = hosted;
        return hosted;
    }

    /**
     * Names the engine's player listener: it accepts the gateway on {@link #playerAddress()} at
     * {@code port}, speaking Minecraft protocol {@code protocol}. The engine must leave compression
     * and encryption to the gateway.
     */
    public synchronized void bind(int port, int protocol) {
        var hosted = sessions;
        if (hosted == null) throw new IllegalStateException("Host sessions before binding");
        if (port < 1 || port > 65_535) throw new IllegalArgumentException("Invalid port");
        var address = playerAddress();
        var host =
                address instanceof Inet6Address
                        ? "[" + address.getHostAddress() + "]"
                        : address.getHostAddress();
        bind(host + ":" + port, protocol, hosted.state());
    }

    /** Reports session and delivery changes to core. */
    void flush() {
        var current = link;
        if (current != null) current.wake();
    }

    /** Sends a session method's result to core. */
    void methodResult(String operation, JvmMethodResult result) {
        var current = link;
        if (current != null) current.methodResult(operation, result);
    }

    /** Stops accepting new work and notifies the app's shutdown handler. */
    public void requestShutdown() {
        health.drain();
        shutdown.complete(null);
    }

    /** Stops accepting new work and fails the shutdown request, unless one was already made. */
    private void fail(RuntimeException error) {
        health.drain();
        if (shutdown.completeExceptionally(error))
            LOG.log(
                    System.Logger.Level.ERROR,
                    "Core refused the JVM for good; shutting down",
                    error);
    }

    public boolean isReady() {
        return health.acceptsWork();
    }

    /** The address the engine's player listener binds; only the gateway connects to it. */
    public InetAddress playerAddress() {
        return environment.playerAddress();
    }

    /** Engine adapters bind their player endpoint and state once, before application readiness. */
    synchronized void bind(String playerEndpoint, int protocol, ProcessState state) {
        if (closed || this.state != null)
            throw new IllegalStateException("Process already bound or closed");
        this.playerEndpoint = playerEndpoint;
        this.protocol = protocol;
        this.state = state;
    }

    /**
     * Marks application initialization complete, and waits for core to accept this launch.
     *
     * @throws IllegalStateException if core refuses the credential or launch
     */
    public void ready() {
        CoreLink started;
        synchronized (this) {
            if (closed || state == null) throw new IllegalStateException("Engine must be started");
            if (link != null) return;
            health.ready(true);
            started =
                    new CoreLink(
                            environment.coreEndpoint(),
                            environment.processToken(),
                            JvmRegistration.newBuilder()
                                    .setProcessId(environment.processId())
                                    .setGeneration(environment.processGeneration())
                                    .setApp(environment.appId())
                                    .setProfile(environment.machineProfile())
                                    .setArtifactDigest(environment.artifactDigest())
                                    .setDeployment(environment.deployment())
                                    .setPlayerEndpoint(playerEndpoint)
                                    .setProtocol(protocol)
                                    .build(),
                            state,
                            health::snapshot,
                            this::requestShutdown,
                            this::fail);
            link = started;
        }
        try {
            started.start();
        } catch (RuntimeException error) {
            synchronized (this) {
                health.ready(false);
                if (link == started) link = null;
            }
            started.close();
            throw error;
        }
    }

    @Override
    public synchronized void close() {
        if (closed) return;
        closed = true;
        health.drain();
        shutdown.complete(null);
        var current = link;
        if (current != null) current.close();
        var hosted = sessions;
        if (hosted != null) hosted.close();
        backend.close();
    }
}
