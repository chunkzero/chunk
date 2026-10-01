package dev.chunkzero.runtime.bootstrap;

import chunk.sync.v1.CoreGrpc;
import chunk.sync.v1.CoreOuterClass.CallRequest;
import chunk.sync.v1.CoreOuterClass.CallResponse;
import chunk.sync.v1.Jvm.JvmMove;
import chunk.sync.v1.Jvm.JvmMoveResult;

import com.google.protobuf.InvalidProtocolBufferException;

import dev.chunkzero.backend.api.SessionId;
import dev.chunkzero.backend.client.BackendSession;
import dev.chunkzero.backend.client.SessionIdentity;
import dev.chunkzero.runtime.control.CoreChannel;

import io.grpc.ManagedChannel;
import io.grpc.Metadata;
import io.grpc.stub.MetadataUtils;
import io.grpc.stub.StreamObserver;

import org.jetbrains.annotations.ApiStatus;

import java.time.Duration;
import java.util.Optional;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.Executors;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.TimeUnit;

@ApiStatus.Internal
public final class SessionBackend implements AutoCloseable {
    private final ManagedChannel channel;
    private final String credential;
    private final String deployment;
    private final CoreGrpc.CoreStub core;
    private final ScheduledExecutorService scheduler = Executors.newSingleThreadScheduledExecutor();

    SessionBackend(ManagedChannel channel, String credential, String deployment) {
        this.channel = channel;
        this.credential = credential;
        this.deployment = deployment;
        var metadata = new Metadata();
        metadata.put(
                Metadata.Key.of("authorization", Metadata.ASCII_STRING_MARSHALLER),
                "Bearer " + credential);
        core =
                CoreGrpc.newStub(channel)
                        .withInterceptors(MetadataUtils.newAttachHeadersInterceptor(metadata));
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

    /**
     * Calls {@code chunk:move} under {@code operation}. Fails if core can't be reached or refuses
     * the call itself, such as for a player another JVM hosts.
     */
    public CompletableFuture<JvmMoveResult> move(String operation, JvmMove move) {
        var result = new CompletableFuture<JvmMoveResult>();
        var request =
                CallRequest.newBuilder()
                        .setOperationId(operation)
                        .setMethod("chunk:move")
                        .setArguments(move.toByteString())
                        .build();
        core.withDeadlineAfter(5, TimeUnit.SECONDS)
                .call(
                        request,
                        new StreamObserver<>() {
                            @Override
                            public void onNext(CallResponse response) {
                                if (response.hasError()) {
                                    result.completeExceptionally(
                                            new IllegalStateException(
                                                    "Core refused the move: "
                                                            + response.getError().getMessage()));
                                    return;
                                }
                                try {
                                    result.complete(JvmMoveResult.parseFrom(response.getResult()));
                                } catch (InvalidProtocolBufferException error) {
                                    result.completeExceptionally(error);
                                }
                            }

                            @Override
                            public void onError(Throwable error) {
                                result.completeExceptionally(error);
                            }

                            @Override
                            public void onCompleted() {
                                result.completeExceptionally(
                                        new IllegalStateException("Core sent no move result"));
                            }
                        });
        return result;
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
