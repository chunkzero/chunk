package dev.chunkzero.runtime.bootstrap;

import chunk.v1.BackendGrpc;
import chunk.v1.Common.DeploymentRef;

import com.google.protobuf.Empty;

import dev.chunkzero.backend.api.SessionId;
import dev.chunkzero.backend.client.BackendSession;
import dev.chunkzero.backend.client.SessionIdentity;

import io.grpc.ManagedChannel;
import io.grpc.Metadata;
import io.grpc.netty.shaded.io.grpc.netty.NettyChannelBuilder;
import io.grpc.stub.MetadataUtils;

import org.jetbrains.annotations.ApiStatus;

import java.net.InetAddress;
import java.net.URI;
import java.net.UnknownHostException;
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

    public static SessionBackend fromEnvironment(
            DeploymentRef deployment, RuntimeEnvironment environment) throws UnknownHostException {
        checkDeployment(deployment, environment);
        return new SessionBackend(
                channel(environment.controlEndpoint()),
                environment.processToken(),
                deployment.getDeployment());
    }

    /**
     * Fails fast when the backend doesn't serve the deployment. Core has no check a JVM may call
     * yet, so this asks the {@code chunk.v1.Backend} service.
     */
    private static void checkDeployment(DeploymentRef deployment, RuntimeEnvironment environment)
            throws UnknownHostException {
        var credential = environment.backendToken();
        if (credential == null)
            throw new IllegalArgumentException("CHUNK_BACKEND_TOKEN is required");
        var channel = channel(environment.backendEndpoint());
        try {
            var metadata = new Metadata();
            metadata.put(
                    Metadata.Key.of("authorization", Metadata.ASCII_STRING_MARSHALLER),
                    "Bearer " + credential);
            metadata.put(
                    Metadata.Key.of("x-chunk-environment", Metadata.ASCII_STRING_MARSHALLER),
                    deployment.getEnvironment());
            metadata.put(
                    Metadata.Key.of("x-chunk-deployment", Metadata.ASCII_STRING_MARSHALLER),
                    deployment.getDeployment());
            BackendGrpc.newBlockingStub(channel)
                    .withInterceptors(MetadataUtils.newAttachHeadersInterceptor(metadata))
                    .withDeadlineAfter(5, TimeUnit.SECONDS)
                    .checkDeployment(Empty.getDefaultInstance());
        } finally {
            channel.shutdownNow();
        }
    }

    private static ManagedChannel channel(String endpoint) throws UnknownHostException {
        var uri = URI.create(endpoint);
        if (!"http".equals(uri.getScheme())
                || uri.getHost() == null
                || uri.getRawQuery() != null
                || uri.getFragment() != null
                || uri.getUserInfo() != null
                || (uri.getPath() != null && !uri.getPath().isEmpty() && !uri.getPath().equals("/"))
                || uri.getPort() < 1
                || uri.getPort() > 65535
                || !InetAddress.getByName(uri.getHost()).isLoopbackAddress()) {
            throw new IllegalArgumentException(
                    "Backend and control endpoints must be loopback HTTP addresses");
        }
        return NettyChannelBuilder.forAddress(uri.getHost(), uri.getPort()).usePlaintext().build();
    }
}
