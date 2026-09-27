package dev.chunkzero.runtime.bootstrap;

import dev.chunkzero.backend.api.SessionId;
import dev.chunkzero.backend.client.BackendSession;
import dev.chunkzero.backend.client.SessionIdentity;
import dev.chunkzero.runtime.control.CoreChannel;

import io.grpc.ManagedChannel;

import org.jetbrains.annotations.ApiStatus;

import java.time.Duration;
import java.util.Optional;
import java.util.concurrent.Executors;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.TimeUnit;

@ApiStatus.Internal
public final class SessionBackend implements AutoCloseable {
    private final ManagedChannel channel;
    private final String credential;
    private final String deployment;
    private final ScheduledExecutorService scheduler = Executors.newSingleThreadScheduledExecutor();

    SessionBackend(ManagedChannel channel, String credential, String deployment) {
        this.channel = channel;
        this.credential = credential;
        this.deployment = deployment;
    }

    /** A session's backend, calling core over the sync protocol with the process credential. */
    public BackendSession client(String id, String app) {
        return BackendSession.overCore(
                channel,
                credential,
                deployment,
                new SessionIdentity(new SessionId(id), app, Optional.empty()),
                scheduler,
                Duration.ofSeconds(5));
    }

    @Override
    public void close() {
        try {
            channel.shutdownNow().awaitTermination(3, TimeUnit.SECONDS);
        } catch (InterruptedException ignored) {
            Thread.currentThread().interrupt();
        } finally {
            scheduler.shutdownNow();
        }
    }

    public static SessionBackend fromEnvironment(RuntimeEnvironment environment) {
        return new SessionBackend(
                CoreChannel.open(environment.coreEndpoint()),
                environment.processToken(),
                environment.deployment());
    }
}
