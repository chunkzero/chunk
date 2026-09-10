package dev.chunkzero.runtime;

import chunk.v1.ProcessControlGrpc;
import chunk.v1.Supervision.ProcessIdentity;
import chunk.v1.Supervision.ProcessInventory;
import chunk.v1.Supervision.SessionCommand;
import chunk.v1.Supervision.SessionInventory;

import io.grpc.Status;
import io.grpc.stub.StreamObserver;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicLong;
import java.util.function.Supplier;

final class ProcessService extends ProcessControlGrpc.ProcessControlImplBase {
    private final ProcessIdentity identity;
    private final GameplayService gameplay;
    private final SessionManager sessions;
    private final AtomicLong ticks;
    private final CountDownLatch shutdown;

    ProcessService(
            ProcessIdentity identity,
            GameplayService gameplay,
            SessionManager sessions,
            AtomicLong ticks,
            CountDownLatch shutdown) {
        this.identity = identity;
        this.gameplay = gameplay;
        this.sessions = sessions;
        this.ticks = ticks;
        this.shutdown = shutdown;
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
        response.onNext(
                ProcessInventory.newBuilder()
                        .setIdentity(identity)
                        .setTickCount(ticks.get())
                        .addAllDeliveries(gameplay.deliveries())
                        .addAllSessions(gameplay.sessions())
                        .build());
        response.onCompleted();
    }

    @Override
    public void createSession(SessionCommand request, StreamObserver<SessionInventory> response) {
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

    @Override
    public void stopProcess(ProcessIdentity request, StreamObserver<ProcessIdentity> response) {
        if (!request.equals(identity)) {
            response.onError(
                    Status.FAILED_PRECONDITION
                            .withDescription("Stale process identity")
                            .asRuntimeException());
            return;
        }
        response.onNext(identity);
        response.onCompleted();
        Thread.startVirtualThread(
                () -> {
                    try {
                        Thread.sleep(100);
                    } catch (InterruptedException ignored) {
                        Thread.currentThread().interrupt();
                    } finally {
                        shutdown.countDown();
                    }
                });
    }
}
