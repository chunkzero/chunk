package dev.chunkzero.runtime;

import chunk.v1.Supervision.ProcessRegistration;
import chunk.v1.SupervisorGrpc;

import io.grpc.ManagedChannel;
import io.grpc.Metadata;
import io.grpc.netty.shaded.io.grpc.netty.NettyChannelBuilder;
import io.grpc.stub.MetadataUtils;

import java.net.URI;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;

/** Reattachment repeats the frozen registration; it never disposes gameplay. */
final class Registration implements AutoCloseable {
    private final ManagedChannel channel;
    private final AtomicBoolean closed = new AtomicBoolean();
    private final Thread worker;

    Registration(String endpoint, String token, ProcessRegistration registration) {
        var address = URI.create(endpoint);
        if (!"127.0.0.1".equals(address.getHost())
                || address.getPort() <= 0
                || !"http".equals(address.getScheme())) {
            throw new IllegalArgumentException(
                    "Supervisor endpoint must be a loopback HTTP address");
        }
        channel =
                NettyChannelBuilder.forAddress(address.getHost(), address.getPort())
                        .usePlaintext()
                        .build();
        worker = Thread.startVirtualThread(() -> register(token, registration));
    }

    private void register(String token, ProcessRegistration registration) {
        var metadata = new Metadata();
        metadata.put(
                Metadata.Key.of("authorization", Metadata.ASCII_STRING_MARSHALLER),
                "Bearer " + token);
        var client =
                SupervisorGrpc.newBlockingStub(channel)
                        .withInterceptors(MetadataUtils.newAttachHeadersInterceptor(metadata));
        while (!closed.get()) {
            try {
                if (!client.withDeadlineAfter(3, TimeUnit.SECONDS)
                        .registerProcess(registration)
                        .equals(registration.getIdentity())) {
                    throw new IllegalStateException(
                            "Supervisor returned a different process identity");
                }
            } catch (Exception ignored) {
                // Existing authenticated TCP deliveries retain their original ownership.
            }
            try {
                Thread.sleep(1000);
            } catch (InterruptedException ignored) {
                break;
            }
        }
    }

    @Override
    public void close() {
        closed.set(true);
        channel.shutdownNow();
        worker.interrupt();
    }
}
