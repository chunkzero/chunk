package dev.chunkzero.runtime.bootstrap;

import chunk.sync.v1.CoreGrpc;
import chunk.sync.v1.CoreOuterClass.CallRequest;
import chunk.sync.v1.CoreOuterClass.CallResponse;
import chunk.sync.v1.CoreOuterClass.Error;
import chunk.sync.v1.Jvm.JvmMove;
import chunk.sync.v1.Jvm.JvmMoveResult;

import com.google.protobuf.InvalidProtocolBufferException;

import dev.chunkzero.backend.api.SessionId;
import dev.chunkzero.backend.client.BackendSession;
import dev.chunkzero.backend.client.SessionIdentity;
import dev.chunkzero.runtime.control.CoreChannel;

import io.grpc.ManagedChannel;
import io.grpc.Metadata;
import io.grpc.Status;
import io.grpc.stub.MetadataUtils;
import io.grpc.stub.StreamObserver;

import org.jetbrains.annotations.ApiStatus;
import org.jetbrains.annotations.Nullable;

import java.time.Duration;
import java.util.Objects;
import java.util.Optional;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.Executors;
import java.util.concurrent.RejectedExecutionException;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.TimeUnit;

@ApiStatus.Internal
public final class SessionBackend implements AutoCloseable {
    /** Waits before each repeat of a move whose call may not have reached core. */
    private static final long[] RETRY_MILLIS = {100, 250, 500, 1000};

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
     * Calls {@code chunk:move} under {@code operation}, repeating it while core can't be reached or
     * its reply is lost, since a repeat finds a move core already queued. Fails once the retries
     * run out, or if core refuses the call itself, such as for a player another JVM hosts.
     */
    public CompletableFuture<JvmMoveResult> move(String operation, JvmMove move) {
        var result = new CompletableFuture<JvmMoveResult>();
        var request =
                CallRequest.newBuilder()
                        .setOperationId(operation)
                        .setMethod("chunk:move")
                        .setArguments(move.toByteString())
                        .build();
        attempt(request, 0, result);
        return result;
    }

    private void attempt(
            CallRequest request, int retries, CompletableFuture<JvmMoveResult> result) {
        call(request)
                .whenComplete(
                        (response, error) -> {
                            if (error == null && !response.hasError()) {
                                try {
                                    result.complete(JvmMoveResult.parseFrom(response.getResult()));
                                } catch (InvalidProtocolBufferException invalid) {
                                    result.completeExceptionally(invalid);
                                }
                                return;
                            }
                            var failure =
                                    error != null
                                            ? error
                                            : new IllegalStateException(
                                                    "Core refused the move: "
                                                            + response.getError().getMessage());
                            if (retries >= RETRY_MILLIS.length || !retryable(response, error)) {
                                result.completeExceptionally(failure);
                                return;
                            }
                            try {
                                scheduler.schedule(
                                        () -> attempt(request, retries + 1, result),
                                        RETRY_MILLIS[retries],
                                        TimeUnit.MILLISECONDS);
                            } catch (RejectedExecutionException closed) {
                                result.completeExceptionally(failure);
                            }
                        });
    }

    private CompletableFuture<CallResponse> call(CallRequest request) {
        var response = new CompletableFuture<CallResponse>();
        core.withDeadlineAfter(5, TimeUnit.SECONDS)
                .call(
                        request,
                        new StreamObserver<>() {
                            @Override
                            public void onNext(CallResponse value) {
                                response.complete(value);
                            }

                            @Override
                            public void onError(Throwable error) {
                                response.completeExceptionally(error);
                            }

                            @Override
                            public void onCompleted() {
                                response.completeExceptionally(
                                        new IllegalStateException("Core sent no response"));
                            }
                        });
        return response;
    }

    /** Whether a repeat of the call may succeed: core was unreachable, busy, or its reply lost. */
    private static boolean retryable(@Nullable CallResponse response, @Nullable Throwable error) {
        if (error != null) {
            var code = Status.fromThrowable(error).getCode();
            return code == Status.Code.UNAVAILABLE || code == Status.Code.DEADLINE_EXCEEDED;
        }
        var code = Objects.requireNonNull(response).getError().getCode();
        return code == Error.Code.CODE_UNAVAILABLE || code == Error.Code.CODE_OVERLOADED;
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
