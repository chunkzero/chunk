package dev.chunkzero.runtime;

import chunk.v1.Common.DeploymentRef;
import chunk.v1.Common.SessionRef;
import chunk.v1.Supervision.ProcessIdentity;
import chunk.v1.Supervision.ProcessRegistration;
import chunk.v1.Supervision.SessionCommand;

import dev.chunkzero.runtime.bootstrap.AppRegistry;
import dev.chunkzero.runtime.bootstrap.FlatSession;
import dev.chunkzero.runtime.bootstrap.RuntimeEnvironment;
import dev.chunkzero.runtime.bootstrap.SessionBackend;
import dev.chunkzero.runtime.control.ProcessAuthentication;
import dev.chunkzero.runtime.control.ProcessService;
import dev.chunkzero.runtime.control.Registration;
import dev.chunkzero.runtime.delivery.GameplayService;

import io.grpc.netty.shaded.io.grpc.netty.NettyServerBuilder;

import net.minestom.server.MinecraftServer;
import net.minestom.server.timer.TaskSchedule;

import java.net.InetSocketAddress;
import java.util.LinkedHashMap;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;

/** Standalone gameplay process entrypoint. */
public final class BridgeMain {
    private BridgeMain() {}

    public static void main(String[] args) throws Exception {
        var environment = RuntimeEnvironment.load();
        var token = environment.processToken();
        var authentication = new ProcessAuthentication(token);
        var deployment =
                DeploymentRef.newBuilder()
                        .setEnvironment(environment.environment())
                        .setDeployment(environment.deployment())
                        .build();
        var minecraft = MinecraftServer.init();
        MinecraftServer.setCompressionThreshold(0);
        var process = MinecraftServer.process();
        MinecraftServer.getConnectionManager().setPlayerProvider(ManagedPlayer::new);
        var supervisor = environment.supervisor();
        var identity =
                ProcessIdentity.newBuilder()
                        .setDeployment(deployment)
                        .setRuntimeId(environment.runtimeId())
                        .setProcessId(environment.processId())
                        .setGeneration(environment.processGeneration())
                        .setMachineProfile(environment.machineProfile())
                        .setArtifactDigest(environment.artifactDigest())
                        .build();
        var shutdown = new CountDownLatch(1);
        var ticks = new AtomicLong();
        var tickExecutor = new TickExecutor();
        var factories = new LinkedHashMap<String, SessionRegistration>();
        SessionBackend backend;
        try {
            factories.putAll(AppRegistry.load(Thread.currentThread().getContextClassLoader()));
            boolean hasApps = !factories.isEmpty();
            if (environment.bootstrapSession()) {
                if (factories.putIfAbsent(
                                "bridge", new SessionRegistration("bridge", FlatSession::new))
                        != null) {
                    throw new IllegalArgumentException(
                            "Bootstrap fixture conflicts with app bridge");
                }
            }
            backend = SessionBackend.fromEnvironment(deployment, environment, hasApps);
        } catch (Exception | LinkageError error) {
            process.stop();
            throw error;
        }
        var sessions =
                new SessionManager(
                        tickExecutor, factories, backend == null ? null : backend::client);
        var gameplay =
                new GameplayService(
                        deployment,
                        identity.getGeneration(),
                        sessions,
                        System::nanoTime,
                        identity.getRuntimeId());
        var server =
                NettyServerBuilder.forAddress(
                                new InetSocketAddress("127.0.0.1", supervisor == null ? 25566 : 0))
                        .maxConcurrentCallsPerConnection(128)
                        .maxInboundMessageSize(65_536)
                        .intercept(authentication)
                        .addService(gameplay)
                        .addService(
                                new ProcessService(identity, gameplay, sessions, ticks, shutdown))
                        .build();

        try {
            minecraft.start("127.0.0.1", 0);
            gameplay.setEndpoint("127.0.0.1:" + process.server().getPort());
            server.start();
        } catch (Exception error) {
            server.shutdownNow();
            process.stop();
            gameplay.close();
            if (backend != null) backend.close();
            throw error;
        }
        MinecraftServer.getSchedulerManager()
                .buildTask(
                        () -> {
                            tickExecutor.flush();
                            gameplay.flush();
                            ticks.incrementAndGet();
                        })
                .repeat(TaskSchedule.tick(1))
                .schedule();
        if (environment.bootstrapSession()) {
            sessions.create(
                            SessionCommand.newBuilder()
                                    .setIdentity(identity)
                                    .setOperationId("fixture")
                                    .setSession(SessionRef.newBuilder().setId("bridge"))
                                    .setGeneration(1)
                                    .setSessionType("bridge")
                                    .setCapacity(128)
                                    .build())
                    .get(5, TimeUnit.SECONDS);
        }
        var registration =
                supervisor == null
                        ? null
                        : new Registration(
                                supervisor,
                                token,
                                ProcessRegistration.newBuilder()
                                        .setIdentity(identity)
                                        .setControlEndpoint("127.0.0.1:" + server.getPort())
                                        .setPlayerEndpoint(gameplay.getEndpoint())
                                        .setConfiguration(gameplay.getConfigurationArtifact())
                                        .build());
        var stopped = new AtomicBoolean();
        Runnable close =
                () -> {
                    if (!stopped.compareAndSet(false, true)) return;
                    if (registration != null) registration.close();
                    if (backend != null) backend.close();
                    try {
                        server.shutdownNow().awaitTermination(5, TimeUnit.SECONDS);
                    } catch (InterruptedException ignored) {
                        Thread.currentThread().interrupt();
                    } finally {
                        process.stop();
                        gameplay.close();
                    }
                };
        Runtime.getRuntime().addShutdownHook(new Thread(close));
        System.out.println(
                "Gameplay ready on 127.0.0.1:"
                        + server.getPort()
                        + "; Minecraft listener at "
                        + gameplay.getEndpoint());
        try {
            shutdown.await();
        } finally {
            close.run();
        }
    }
}
