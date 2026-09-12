package dev.chunkzero.runtime;

import chunk.v1.Common.DeploymentRef;
import chunk.v1.NodeControlGrpc;
import chunk.v1.Supervision.ProcessIdentity;
import chunk.v1.Supervision.ProcessRegistration;

import dev.chunkzero.backend.client.BackendSession;
import dev.chunkzero.runtime.bootstrap.RuntimeEnvironment;
import dev.chunkzero.runtime.bootstrap.SessionBackend;
import dev.chunkzero.runtime.control.ProcessAuthentication;
import dev.chunkzero.runtime.control.Registration;

import io.grpc.BindableService;
import io.grpc.Server;
import io.grpc.Status;
import io.grpc.netty.shaded.io.grpc.netty.NettyServerBuilder;
import io.grpc.stub.StreamObserver;

import org.jetbrains.annotations.Nullable;

import java.io.IOException;
import java.net.InetSocketAddress;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionStage;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;

/** Connects one immutable app process to platform control. */
public final class ChunkProcess implements AutoCloseable {
    private final RuntimeEnvironment environment;
    private final ProcessIdentity identity;
    private final SessionBackend backend;
    private final AtomicLong ticks = new AtomicLong();
    private final ProcessHealth health = new ProcessHealth(ticks);
    private final CompletableFuture<Void> shutdown = new CompletableFuture<>();
    private @Nullable Server control;
    private @Nullable Registration registration;
    private String playerEndpoint = "";
    private boolean closed;

    ChunkProcess(RuntimeEnvironment environment) throws IOException {
        this.environment = environment;
        if (!environment.appId().matches("[A-Za-z_][A-Za-z0-9_]{0,127}")
                || environment.processGeneration() < 1)
            throw new IllegalArgumentException("Invalid app or process identity");
        var deployment =
                DeploymentRef.newBuilder()
                        .setEnvironment(environment.environment())
                        .setDeployment(environment.deployment())
                        .build();
        identity =
                ProcessIdentity.newBuilder()
                        .setDeployment(deployment)
                        .setAppId(environment.appId())
                        .setRuntimeId(environment.runtimeId())
                        .setProcessId(environment.processId())
                        .setGeneration(environment.processGeneration())
                        .setMachineProfile(environment.machineProfile())
                        .setArtifactDigest(environment.artifactDigest())
                        .build();
        if (environment.processToken().length() < 32)
            throw new IllegalArgumentException("Invalid process credential");
        backend = SessionBackend.fromEnvironment(deployment, environment);
    }

    public static ChunkProcess connect() throws IOException {
        return new ChunkProcess(RuntimeEnvironment.load());
    }

    public CompletionStage<Void> shutdownRequested() {
        return shutdown.minimalCompletionStage();
    }

    public void awaitShutdown() throws InterruptedException {
        try {
            shutdown.get();
        } catch (java.util.concurrent.ExecutionException error) {
            throw new IllegalStateException(error.getCause());
        }
    }

    ProcessIdentity identity() {
        return identity;
    }

    BackendSession backend(String session) {
        return backend.client(session, environment.appId());
    }

    long tickCount() {
        return ticks.get();
    }

    void progress(int sessions, int players) {
        health.tick(sessions, players);
    }

    /** Stops accepting new work and notifies the app's shutdown handler. */
    public void requestShutdown() {
        health.drain();
        shutdown.complete(null);
    }

    public boolean isReady() {
        return health.acceptsWork();
    }

    /** Engine adapters bind their services once, before application readiness. */
    synchronized void bind(List<BindableService> services, String playerEndpoint)
            throws IOException {
        if (closed || control != null)
            throw new IllegalStateException("Process already bound or closed");
        this.playerEndpoint = playerEndpoint;
        var builder =
                NettyServerBuilder.forAddress(new InetSocketAddress("127.0.0.1", 0))
                        .maxConcurrentCallsPerConnection(128)
                        .maxInboundMessageSize(65_536)
                        .intercept(new ProcessAuthentication(environment.processToken()))
                        .addService(
                                new NodeControlGrpc.NodeControlImplBase() {
                                    @Override
                                    public void health(
                                            ProcessIdentity request,
                                            StreamObserver<chunk.v1.Supervision.ProcessHealth>
                                                    response) {
                                        if (!request.equals(identity)) {
                                            response.onError(
                                                    Status.FAILED_PRECONDITION
                                                            .asRuntimeException());
                                            return;
                                        }
                                        response.onNext(health.snapshot(identity));
                                        response.onCompleted();
                                    }

                                    @Override
                                    public void stopProcess(
                                            ProcessIdentity request,
                                            StreamObserver<ProcessIdentity> response) {
                                        if (!request.equals(identity)) {
                                            response.onError(
                                                    Status.FAILED_PRECONDITION
                                                            .asRuntimeException());
                                            return;
                                        }
                                        requestShutdown();
                                        response.onNext(identity);
                                        response.onCompleted();
                                    }
                                });
        services.forEach(builder::addService);
        control = builder.build();
        try {
            control.start();
        } catch (IOException | RuntimeException error) {
            close();
            throw error;
        }
    }

    /** Marks application initialization complete and waits for control to accept this launch. */
    public synchronized void ready() {
        if (closed || control == null) throw new IllegalStateException("Engine must be started");
        if (registration != null) return;
        health.ready(true);
        try {
            registration =
                    new Registration(
                            environment.controlEndpoint(),
                            environment.processToken(),
                            ProcessRegistration.newBuilder()
                                    .setIdentity(identity)
                                    .setControlEndpoint("http://127.0.0.1:" + control.getPort())
                                    .setPlayerEndpoint(playerEndpoint)
                                    .build());
        } catch (RuntimeException error) {
            health.ready(false);
            throw error;
        }
    }

    @Override
    public synchronized void close() {
        if (closed) return;
        closed = true;
        health.drain();
        shutdown.complete(null);
        if (registration != null) registration.close();
        if (control != null) {
            control.shutdownNow();
            try {
                control.awaitTermination(3, TimeUnit.SECONDS);
            } catch (InterruptedException ignored) {
                Thread.currentThread().interrupt();
            }
        }
        backend.close();
    }
}
