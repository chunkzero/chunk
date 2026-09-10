package dev.chunkzero.runtime;

import chunk.v1.Common.DeploymentRef;
import dev.chunkzero.backend.api.SessionId;
import dev.chunkzero.backend.client.BackendSession;
import dev.chunkzero.backend.client.SessionIdentity;
import io.grpc.ManagedChannel;
import io.grpc.netty.shaded.io.grpc.netty.NettyChannelBuilder;
import java.net.InetAddress;
import java.net.URI;
import java.net.UnknownHostException;
import java.time.Duration;
import java.util.Optional;
import java.util.concurrent.Executors;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.TimeUnit;
import org.jetbrains.annotations.Nullable;

final class SessionBackend implements AutoCloseable {
    private final ManagedChannel channel;
    private final String credential;
    private final DeploymentRef deployment;
    private final ScheduledExecutorService scheduler = Executors.newSingleThreadScheduledExecutor();

    SessionBackend(ManagedChannel channel, String credential, DeploymentRef deployment) {
        this.channel = channel;
        this.credential = credential;
        this.deployment = deployment;
    }

    BackendSession client(String id, String app) {
        return new BackendSession(
                channel,
                credential,
                deployment.getEnvironment(),
                deployment.getDeployment(),
                new SessionIdentity(new SessionId(id), app, Optional.empty()),
                scheduler,
                Duration.ofSeconds(5));
    }

    @Override
    public void close() throws InterruptedException {
        try {
            channel.shutdownNow().awaitTermination(3, TimeUnit.SECONDS);
        } finally {
            scheduler.shutdownNow();
        }
    }

    static @Nullable SessionBackend fromEnvironment(DeploymentRef deployment, RuntimeEnvironment environment)
            throws UnknownHostException {
        var endpoint = environment.backendEndpoint();
        if (endpoint == null) return null;
        var credential = environment.backendToken();
        if (credential == null) throw new IllegalArgumentException("CHUNK_BACKEND_TOKEN is required");
        var uri = URI.create(endpoint);
        if (!"http".equals(uri.getScheme()) || uri.getPort() < 1 || uri.getPort() > 65535
                || !InetAddress.getByName(uri.getHost()).isLoopbackAddress()) {
            throw new IllegalArgumentException("Backend endpoint must be a loopback HTTP address");
        }
        return new SessionBackend(
                NettyChannelBuilder.forAddress(uri.getHost(), uri.getPort()).usePlaintext().build(),
                credential,
                deployment);
    }
}
