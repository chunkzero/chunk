package dev.chunkzero.runtime.minestom.internal;

import chunk.v1.ProcessControlGrpc;
import chunk.v1.Supervision.ProcessIdentity;
import chunk.v1.Supervision.ProcessInventory;
import chunk.v1.Supervision.SessionCommand;
import chunk.v1.Supervision.SessionInventory;

import dev.chunkzero.runtime.ChunkProcess;
import dev.chunkzero.runtime.SessionManager;

import io.grpc.Status;
import io.grpc.stub.StreamObserver;

import org.jetbrains.annotations.ApiStatus;

import java.util.concurrent.CompletableFuture;
import java.util.function.LongSupplier;
import java.util.function.Supplier;

@ApiStatus.Internal
public final class ProcessService extends ProcessControlGrpc.ProcessControlImplBase {
    private final ProcessIdentity identity;
    private final GameplayService gameplay;
    private final SessionManager sessions;
    private final LongSupplier ticks;
    private final ChunkProcess process;

    public ProcessService(
            ProcessIdentity identity,
            GameplayService gameplay,
            SessionManager sessions,
            LongSupplier ticks,
            ChunkProcess process) {
        this.identity = identity;
        this.gameplay = gameplay;
        this.sessions = sessions;
        this.ticks = ticks;
        this.process = process;
    }

    @Override
    public void inventory(ProcessIdentity request, StreamObserver<ProcessInventory> response) {
        if (!request.equals(identity)) {
            response.onError(
                    Status.FAILED_PRECONDITION
                            .withDescription("Stale process identity")
                            .asRuntimeException());
            return;
        }
        // Session creations queued before this call run first, so the inventory includes them.
        sessions.afterQueued()
                .thenRun(
                        () -> {
                            response.onNext(
                                    ProcessInventory.newBuilder()
                                            .setIdentity(identity)
                                            .setTickCount(ticks.getAsLong())
                                            .setDraining(!process.isReady())
                                            .addAllDeliveries(gameplay.deliveries())
                                            .addAllSessions(gameplay.sessions())
                                            .build());
                            response.onCompleted();
                        })
                .exceptionally(
                        error -> {
                            response.onError(Status.INTERNAL.asRuntimeException());
                            return null;
                        });
    }

    @Override
    public void createSession(SessionCommand request, StreamObserver<SessionInventory> response) {
        if (!process.isReady()) {
            response.onError(
                    Status.UNAVAILABLE.withDescription("Server not ready").asRuntimeException());
            return;
        }
        sessionReply(request, response, () -> sessions.create(request));
    }

    @Override
    public void finishSession(SessionCommand request, StreamObserver<SessionInventory> response) {
        sessionReply(request, response, () -> sessions.finish(request));
    }

    private void sessionReply(
            SessionCommand request,
            StreamObserver<SessionInventory> response,
            Supplier<CompletableFuture<SessionInventory>> action) {
        if (!request.getIdentity().equals(identity)) {
            response.onError(Status.FAILED_PRECONDITION.asRuntimeException());
            return;
        }
        action.get()
                .whenComplete(
                        (result, error) -> {
                            if (error != null) {
                                response.onError(
                                        Status.FAILED_PRECONDITION
                                                .withDescription("Session operation failed")
                                                .asRuntimeException());
                            } else {
                                response.onNext(result);
                                response.onCompleted();
                            }
                        });
    }
}
