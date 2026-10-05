package com.chunkzero.chunk.backend.client;

import chunk.sync.v1.CoreOuterClass.CallResponse;

import com.chunkzero.chunk.backend.api.ActionRef;
import com.chunkzero.chunk.backend.api.FunctionRef;
import com.chunkzero.chunk.backend.api.JsonType;
import com.chunkzero.chunk.backend.api.MutationRef;
import com.chunkzero.chunk.backend.api.PlayerId;
import com.chunkzero.chunk.backend.api.QueryRef;
import com.google.protobuf.ByteString;

import io.grpc.Channel;
import io.grpc.Context;
import io.grpc.stub.StreamObserver;

import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.Objects;
import java.util.Optional;
import java.util.Set;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.function.Consumer;

/** Owns calls and watches; its channel and scheduler belong to the parent runtime. */
public final class BackendSession implements AutoCloseable {
    // Set while this thread closes a session, so a close it causes doesn't wait.
    private static final ThreadLocal<Boolean> CLOSING = ThreadLocal.withInitial(() -> false);
    final Transport transport;
    final SessionIdentity identity;
    final ScheduledExecutorService scheduler;
    final Set<GroupSubscription> watches = ConcurrentHashMap.newKeySet();
    final Dispatcher dispatcher = new Dispatcher();
    final AtomicBoolean closed = new AtomicBoolean();
    private final Set<CompletableFuture<?>> calls = ConcurrentHashMap.newKeySet();
    private final Set<ActionCall<?>> actions = ConcurrentHashMap.newKeySet();
    private final Set<BackendSession> children = ConcurrentHashMap.newKeySet();
    private final Duration deadline;

    private BackendSession(
            Transport transport,
            SessionIdentity identity,
            ScheduledExecutorService scheduler,
            Duration deadline) {
        if (deadline.isNegative()
                || deadline.isZero()
                || deadline.compareTo(Duration.ofMinutes(5)) > 0)
            throw new IllegalArgumentException("Invalid call deadline");
        this.transport = transport;
        this.identity = Objects.requireNonNull(identity);
        this.scheduler = Objects.requireNonNull(scheduler);
        this.deadline = deadline;
    }

    /**
     * A session calling core over the sync protocol with a JVM's process credential. Core checks
     * that the JVM runs the identity's session in {@code deployment}, and derives the caller app
     * code sees from it.
     */
    public static BackendSession overCore(
            Channel channel,
            String credential,
            String deployment,
            SessionIdentity identity,
            ScheduledExecutorService scheduler,
            Duration deadline) {
        return new BackendSession(
                new CoreTransport(channel, credential, deployment), identity, scheduler, deadline);
    }

    public BackendSession forPlayer(PlayerId player) {
        var child =
                new BackendSession(
                        transport,
                        new SessionIdentity(
                                identity.session(), identity.app(), Optional.of(player)),
                        scheduler,
                        deadline);
        children.removeIf(BackendSession::finished);
        children.add(child);
        if (closed.get()) child.close();
        return child;
    }

    public <A, R> CompletableFuture<R> query(QueryRef<A, R> reference, A arguments) {
        return call(reference.result(), invocation(reference, arguments), "");
    }

    public <A, R> CompletableFuture<R> mutate(
            MutationRef<A, R> reference, A arguments, OperationId operation) {
        return call(reference.result(), invocation(reference, arguments), operation.value());
    }

    /**
     * Runs a public action at most once, under an operation ID core issues, repeating the call
     * while core can't be reached or its reply is lost, for up to 65 seconds. Fails with {@link
     * OutcomeUnknownException} when the action may or may not have run, including when the session
     * closes after sending it. Cancelling the future only stops waiting; the action runs on.
     */
    public <A, R> CompletableFuture<R> perform(ActionRef<A, R> reference, A arguments) {
        var action = new ActionCall<>(this, invocation(reference, arguments), reference.result());
        actions.add(action);
        action.result.whenComplete((value, error) -> actions.remove(action));
        if (closed.get()) action.result.cancel(false);
        else action.start();
        return action.result;
    }

    public <A, R> BoundQuery<R> bind(QueryRef<A, R> reference, A arguments) {
        return new BoundQuery<>(this, invocation(reference, arguments), reference.result());
    }

    public <A, R> AutoCloseable watch(
            QueryRef<A, R> reference, A arguments, Consumer<WatchState<R>> observer) {
        var query = bind(reference, arguments);
        return watchGroup(
                List.of(query),
                state ->
                        observer.accept(
                                new WatchState<>(
                                        state.stale(),
                                        state.snapshot()
                                                .map(
                                                        snapshot ->
                                                                new WatchState.Snapshot<>(
                                                                        snapshot.revision(),
                                                                        snapshot.result(query))),
                                        state.error())));
    }

    public AutoCloseable watchGroup(List<BoundQuery<?>> queries, Consumer<GroupState> observer) {
        if (queries.isEmpty()
                || queries.size() > 16
                || queries.stream().anyMatch(query -> query.owner != this))
            throw new IllegalArgumentException("Invalid session query group");
        var watch = new GroupSubscription(this, List.copyOf(queries), observer);
        watches.add(watch);
        if (closed.get()) watch.close();
        else watch.start();
        return watch;
    }

    private static <A, R> Transport.Invocation invocation(
            FunctionRef<A, R> reference, A arguments) {
        var encoded = ByteString.copyFromUtf8(reference.arguments().write(arguments));
        if (encoded.size() > 1024 * 1024) throw new IllegalArgumentException("Argument size limit");
        return new Transport.Invocation(reference.path(), encoded);
    }

    private <R> CompletableFuture<R> call(
            JsonType<R> resultType, Transport.Invocation invocation, String operation) {
        var result = new CompletableFuture<R>();
        calls.add(result);
        result.whenComplete((value, error) -> calls.remove(result));
        if (closed.get()) {
            result.cancel(false);
            return result;
        }
        var context = Context.current().withCancellation();
        result.whenComplete((value, error) -> context.cancel(null));
        try {
            context.run(
                    () ->
                            transport.call(
                                    invocation,
                                    identity,
                                    operation,
                                    deadline,
                                    new StreamObserver<CallResponse>() {
                                        private CallResponse response;

                                        public void onNext(CallResponse value) {
                                            response = value;
                                        }

                                        public void onError(Throwable error) {
                                            result.completeExceptionally(error);
                                        }

                                        public void onCompleted() {
                                            try {
                                                result.complete(resultType.read(json(response)));
                                            } catch (RuntimeException error) {
                                                result.completeExceptionally(error);
                                            }
                                        }
                                    }));
        } catch (RuntimeException error) {
            result.completeExceptionally(error);
        }
        return result;
    }

    static String json(CallResponse response) {
        if (response == null) throw new IllegalStateException("Missing backend result");
        return switch (response.getOutcomeCase()) {
            case ERROR -> throw CoreTransport.status(response.getError()).asRuntimeException();
            case RESULT -> {
                if (!response.getResult().isValidUtf8())
                    throw new IllegalArgumentException("Invalid backend JSON");
                yield response.getResult().toStringUtf8();
            }
            default -> throw new IllegalStateException("Missing backend result");
        };
    }

    /**
     * Closes the session and its player children, and returns once none of their callbacks runs.
     * Closing from inside any session's callback, or from a handler that another close runs,
     * doesn't wait; the outer closer does. Waiting continues through interrupts, which it restores.
     */
    @Override
    public void close() {
        var threads = new ArrayList<Thread>();
        boolean nested = CLOSING.get();
        CLOSING.set(true);
        try {
            shut(threads);
        } finally {
            CLOSING.set(nested);
        }
        if (!mayWait()) return;
        boolean interrupted = false;
        for (var thread : threads) {
            while (true) {
                try {
                    thread.join();
                    break;
                } catch (InterruptedException error) {
                    interrupted = true;
                }
            }
        }
        if (interrupted) Thread.currentThread().interrupt();
    }

    /** Whether this thread may wait for callbacks: it is neither closing nor running one. */
    static boolean mayWait() {
        return !CLOSING.get() && !Dispatcher.dispatching();
    }

    /** Closes this subtree without waiting, collecting its callback threads. */
    private void shut(List<Thread> threads) {
        closed.set(true);
        children.forEach(child -> child.shut(threads));
        watches.forEach(GroupSubscription::shut);
        var thread = dispatcher.stop();
        if (thread != null) threads.add(thread);
        calls.forEach(call -> call.cancel(false));
        actions.forEach(ActionCall::close);
    }

    private boolean finished() {
        return closed.get()
                && dispatcher.terminated()
                && children.stream().allMatch(BackendSession::finished);
    }
}
