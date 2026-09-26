package dev.chunkzero.runtime.control;

import chunk.v1.Supervision.ProcessRegistration;
import chunk.v1.SupervisorGrpc;

import io.grpc.ManagedChannel;
import io.grpc.Metadata;
import io.grpc.netty.shaded.io.grpc.netty.NettyChannelBuilder;
import io.grpc.stub.MetadataUtils;

import org.jetbrains.annotations.ApiStatus;

import java.net.URI;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;

/**
 * Reattachment repeats the frozen registration and reopens the process's stream to control; it
 * never disposes gameplay.
 */
@ApiStatus.Internal
public final class Registration implements AutoCloseable {
    private final ManagedChannel channel;
    private final ProcessSync sync;
    private final AtomicBoolean closed = new AtomicBoolean();
    private final Thread worker;

    public Registration(
            String endpoint, String token, ProcessRegistration registration, ProcessState state) {
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
        try {
            if (!client(token).registerProcess(registration).equals(registration.getIdentity()))
                throw new IllegalStateException("Control returned a different process identity");
        } catch (RuntimeException error) {
            channel.shutdownNow();
            throw error;
        }
        sync = new ProcessSync(channel, token, registration.getIdentity(), state);
        sync.open();
        worker = Thread.startVirtualThread(() -> register(token, registration));
    }

    private chunk.v1.SupervisorGrpc.SupervisorBlockingStub client(String token) {
        var metadata = new Metadata();
        metadata.put(
                Metadata.Key.of("authorization", Metadata.ASCII_STRING_MARSHALLER),
                "Bearer " + token);
        return SupervisorGrpc.newBlockingStub(channel)
                .withInterceptors(MetadataUtils.newAttachHeadersInterceptor(metadata))
                .withDeadlineAfter(3, TimeUnit.SECONDS);
    }

    private void register(String token, ProcessRegistration registration) {
        while (!closed.get()) {
            try {
                if (!client(token)
                        .registerProcess(registration)
                        .equals(registration.getIdentity())) {
                    throw new IllegalStateException(
                            "Supervisor returned a different process identity");
                }
                sync.open();
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

    /** Reports session and delivery changes to control. Call from the engine's tick thread. */
    public void flush() {
        sync.flush();
    }

    @Override
    public void close() {
        closed.set(true);
        channel.shutdownNow();
        worker.interrupt();
    }
}
