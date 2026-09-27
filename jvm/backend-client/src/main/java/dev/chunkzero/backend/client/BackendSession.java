package dev.chunkzero.backend.client;

import chunk.sync.v1.CoreOuterClass.CallResponse;

import com.google.protobuf.ByteString;

import dev.chunkzero.backend.api.FunctionRef;
import dev.chunkzero.backend.api.JsonType;
import dev.chunkzero.backend.api.MutationRef;
import dev.chunkzero.backend.api.PlayerId;
import dev.chunkzero.backend.api.QueryRef;

import io.grpc.Channel;
import io.grpc.Context;
import io.grpc.stub.StreamObserver;

import java.time.Duration;
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
    final Transport transport;
    final SessionIdentity identity;
    final ScheduledExecutorService scheduler;
    final Set<GroupSubscription> watches = ConcurrentHashMap.newKeySet();
    final AtomicBoolean closed = new AtomicBoolean();
    private final Set<CompletableFuture<?>> calls = ConcurrentHashMap.newKeySet();
    private final Set<BackendSession> children = ConcurrentHashMap.newKeySet();
    private final Duration deadline;
    private final Runnable onClose;

    /** A session on the {@code chunk.v1.Backend} service, which sends its own caller identity. */
    public BackendSession(
            Channel channel,
            String credential,
            String environment,
            String deployment,
            SessionIdentity identity,
            ScheduledExecutorService scheduler,
            Duration deadline) {
        this(
                new LegacyTransport(channel, credential, environment, deployment),
                identity,
                scheduler,
                deadline,
                () -> {});
    }

    private BackendSession(
            Transport transport,
            SessionIdentity identity,
            ScheduledExecutorService scheduler,
            Duration deadline,
            Runnable onClose) {
        if (deadline.isNegative()
                || deadline.isZero()
                || deadline.compareTo(Duration.ofMinutes(5)) > 0)
            throw new IllegalArgumentException("Invalid call deadline");
        this.transport = transport;
        this.identity = Objects.requireNonNull(identity);
        this.scheduler = Objects.requireNonNull(scheduler);
        this.deadline = deadline;
        this.onClose = onClose;
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
                new CoreTransport(channel, credential, deployment),
                identity,
                scheduler,
                deadline,
                () -> {});
    }

    public BackendSession forPlayer(PlayerId player) {
        var child =
                new BackendSession(
                        transport,
                        new SessionIdentity(
                                identity.session(), identity.app(), Optional.of(player)),
                        scheduler,
                        deadline,
                        () -> children.removeIf(scope -> scope.closed.get()));
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

    private static String json(CallResponse response) {
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

    @Override
    public void close() {
        if (!closed.compareAndSet(false, true)) return;
        children.forEach(BackendSession::close);
        calls.forEach(call -> call.cancel(false));
        watches.forEach(GroupSubscription::close);
        onClose.run();
    }
}
