package dev.chunkzero.backend.client;

import chunk.sync.v1.CoreOuterClass.CallResponse;
import chunk.sync.v1.CoreOuterClass.Error;
import chunk.sync.v1.CoreOuterClass.PrepareResult;

import com.google.protobuf.InvalidProtocolBufferException;

import dev.chunkzero.backend.api.JsonType;

import io.grpc.Context;
import io.grpc.Status;
import io.grpc.stub.StreamObserver;

import java.time.Duration;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.RejectedExecutionException;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.function.BiConsumer;
import java.util.function.Consumer;

/**
 * One action call. Core issues its operation ID, and the call repeats under that ID while core
 * can't be reached, is busy or loses its reply, since a repeat finds the run an earlier attempt
 * started. Attempts stop once {@link #LIMIT} passes.
 */
final class ActionCall<R> {
    /** Core's 30 s action limit, its 30 s worst case to admit one, and slack. */
    static final Duration LIMIT = Duration.ofSeconds(65);

    /** Waits before each repeat; later repeats wait as long as the last. */
    private static final long[] RETRY_MILLIS = {100, 250, 500, 1000};

    // The call's stages, which only move forward, so close never races the first send.
    private static final int PREPARING = 0;
    private static final int CALLING = 1;
    private static final int CLOSED = 2;

    final CompletableFuture<R> result = new CompletableFuture<>();
    private final BackendSession session;
    private final Transport.Invocation invocation;
    private final JsonType<R> resultType;
    private final long deadline = System.nanoTime() + LIMIT.toNanos();
    private final Context.CancellableContext context = Context.current().withCancellation();
    private volatile String operation;

    /** Whether an earlier attempt may have reached core without the client learning its outcome. */
    private volatile boolean maybeSent;

    private final AtomicInteger stage = new AtomicInteger(PREPARING);
    private int retries;

    ActionCall(BackendSession session, Transport.Invocation invocation, JsonType<R> resultType) {
        this.session = session;
        this.invocation = invocation;
        this.resultType = resultType;
        result.whenComplete((value, error) -> context.cancel(null));
    }

    void start() {
        attempt(observer -> session.transport.prepare(remaining(), observer), this::prepared);
    }

    /** Ends the call as its session closes: unknown once it was sent, else cancelled. */
    void close() {
        if (stage.compareAndExchange(PREPARING, CLOSED) == PREPARING) result.cancel(false);
        else
            result.completeExceptionally(
                    new OutcomeUnknownException("The session closed during the action", null));
    }

    private void prepared(CallResponse response, Throwable error) {
        if (error != null) {
            if (unreachable(error)) retry(this::start, error);
            else result.completeExceptionally(error);
        } else if (response.hasError()) {
            var failure = CoreTransport.status(response.getError()).asRuntimeException();
            if (busy(response.getError().getCode())) retry(this::start, failure);
            else result.completeExceptionally(failure);
        } else {
            try {
                operation = PrepareResult.parseFrom(response.getResult()).getOperationId();
            } catch (InvalidProtocolBufferException invalid) {
                result.completeExceptionally(invalid);
                return;
            }
            call();
        }
    }

    private void call() {
        if (stage.compareAndExchange(PREPARING, CALLING) == CLOSED) return;
        attempt(
                observer ->
                        session.transport.call(
                                invocation, session.identity, operation, remaining(), observer),
                this::called);
    }

    private void called(CallResponse response, Throwable error) {
        if (error != null) {
            // Any status but UNAUTHENTICATED leaves open whether core handled the call.
            if (Status.fromThrowable(error).getCode() != Status.Code.UNAUTHENTICATED)
                maybeSent = true;
            if (unreachable(error)) retry(this::call, error);
            else fail(error);
            return;
        }
        if (!response.hasError()) {
            try {
                result.complete(resultType.read(BackendSession.json(response)));
            } catch (RuntimeException invalid) {
                result.completeExceptionally(invalid);
            }
            return;
        }
        var failure = CoreTransport.status(response.getError()).asRuntimeException();
        switch (response.getError().getCode()) {
            case CODE_UNAVAILABLE -> {
                maybeSent = true;
                retry(this::call, failure);
            }
            case CODE_OVERLOADED -> retry(this::call, failure);
            case CODE_OUTCOME_UNKNOWN -> result.completeExceptionally(unknown(failure));
            // Core checks a repeat's caller again, which may since have left.
            case CODE_DENIED, CODE_STOPPED -> fail(failure);
            default -> result.completeExceptionally(failure);
        }
    }

    private void attempt(
            Consumer<StreamObserver<CallResponse>> start,
            BiConsumer<CallResponse, Throwable> handler) {
        if (result.isDone()) return;
        var observer =
                new StreamObserver<CallResponse>() {
                    private CallResponse response;

                    @Override
                    public void onNext(CallResponse value) {
                        response = value;
                    }

                    @Override
                    public void onError(Throwable error) {
                        handler.accept(null, error);
                    }

                    @Override
                    public void onCompleted() {
                        if (response == null)
                            handler.accept(
                                    null, new IllegalStateException("Missing backend result"));
                        else handler.accept(response, null);
                    }
                };
        try {
            context.run(() -> start.accept(observer));
        } catch (RuntimeException error) {
            handler.accept(null, error);
        }
    }

    private void retry(Runnable next, Throwable failure) {
        long delay = RETRY_MILLIS[Math.min(retries++, RETRY_MILLIS.length - 1)];
        if (remaining().toMillis() <= delay) {
            expire(failure);
            return;
        }
        try {
            session.scheduler.schedule(next, delay, TimeUnit.MILLISECONDS);
        } catch (RejectedExecutionException closed) {
            expire(failure);
        }
    }

    private void expire(Throwable failure) {
        if (operation == null)
            result.completeExceptionally(
                    Status.UNAVAILABLE
                            .withDescription("Core issued no operation ID in time")
                            .withCause(failure)
                            .asRuntimeException());
        else fail(failure);
    }

    private void fail(Throwable failure) {
        result.completeExceptionally(maybeSent ? unknown(failure) : failure);
    }

    private Duration remaining() {
        return Duration.ofNanos(Math.max(deadline - System.nanoTime(), 1));
    }

    private static OutcomeUnknownException unknown(Throwable cause) {
        return new OutcomeUnknownException("The action may or may not have run", cause);
    }

    private static boolean unreachable(Throwable error) {
        var code = Status.fromThrowable(error).getCode();
        return code == Status.Code.UNAVAILABLE || code == Status.Code.DEADLINE_EXCEEDED;
    }

    private static boolean busy(Error.Code code) {
        return code == Error.Code.CODE_UNAVAILABLE || code == Error.Code.CODE_OVERLOADED;
    }
}
