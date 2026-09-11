package dev.chunkzero.backend.client;

import chunk.v1.BackendGrpc;
import chunk.v1.BackendOuterClass.BackendMutation;
import chunk.v1.BackendOuterClass.BackendQuery;
import chunk.v1.BackendOuterClass.BackendResult;

import com.google.protobuf.ByteString;

import dev.chunkzero.backend.api.FunctionRef;
import dev.chunkzero.backend.api.JsonType;
import dev.chunkzero.backend.api.MutationRef;
import dev.chunkzero.backend.api.PlayerId;
import dev.chunkzero.backend.api.QueryRef;

import io.grpc.Channel;
import io.grpc.Metadata;
import io.grpc.stub.ClientCallStreamObserver;
import io.grpc.stub.ClientResponseObserver;
import io.grpc.stub.MetadataUtils;

import java.time.Duration;
import java.util.List;
import java.util.Objects;
import java.util.Optional;
import java.util.Set;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.function.Consumer;

/** Owns calls and watches; its channel and scheduler belong to the parent runtime. */
public final class BackendSession implements AutoCloseable {
    final BackendGrpc.BackendStub stub;
    final ScheduledExecutorService scheduler;
    final Set<GroupSubscription> watches = ConcurrentHashMap.newKeySet();
    final AtomicBoolean closed = new AtomicBoolean();
    private final Set<CompletableFuture<?>> calls = ConcurrentHashMap.newKeySet();
    private final Set<BackendSession> children = ConcurrentHashMap.newKeySet();
    private final SessionIdentity identity;
    private final ByteString caller;
    private final Duration deadline;
    private final Runnable onClose;

    public BackendSession(
            Channel channel,
            String credential,
            String environment,
            String deployment,
            SessionIdentity identity,
            ScheduledExecutorService scheduler,
            Duration deadline) {
        if (environment == null
                || environment.isEmpty()
                || environment.length() > 128
                || deployment == null
                || deployment.isEmpty()
                || deployment.length() > 128)
            throw new IllegalArgumentException("Invalid environment or deployment");
        if (deadline.isNegative()
                || deadline.isZero()
                || deadline.compareTo(Duration.ofMinutes(5)) > 0)
            throw new IllegalArgumentException("Invalid call deadline");
        stub = authenticated(channel, credential, environment, deployment);
        this.identity = Objects.requireNonNull(identity);
        this.scheduler = Objects.requireNonNull(scheduler);
        this.deadline = deadline;
        this.onClose = () -> {};
        caller = ByteString.copyFromUtf8(identity.json().toString());
    }

    private BackendSession(BackendSession parent, SessionIdentity identity, Runnable onClose) {
        stub = parent.stub;
        scheduler = parent.scheduler;
        deadline = parent.deadline;
        this.identity = Objects.requireNonNull(identity);
        this.onClose = onClose;
        caller = ByteString.copyFromUtf8(identity.json().toString());
    }

    private static BackendGrpc.BackendStub authenticated(
            Channel channel, String credential, String environment, String deployment) {
        if (credential == null || credential.length() < 32)
            throw new IllegalArgumentException("Invalid backend credential");
        var metadata = new Metadata();
        metadata.put(
                Metadata.Key.of("authorization", Metadata.ASCII_STRING_MARSHALLER),
                "Bearer " + credential);
        metadata.put(
                Metadata.Key.of("x-chunk-environment", Metadata.ASCII_STRING_MARSHALLER),
                environment);
        metadata.put(
                Metadata.Key.of("x-chunk-deployment", Metadata.ASCII_STRING_MARSHALLER),
                deployment);
        return BackendGrpc.newStub(channel)
                .withInterceptors(MetadataUtils.newAttachHeadersInterceptor(metadata));
    }

    public BackendSession forPlayer(PlayerId player) {
        var child =
                new BackendSession(
                        this,
                        new SessionIdentity(
                                identity.session(), identity.app(), Optional.of(player)),
                        () -> children.removeIf(scope -> scope.closed.get()));
        children.add(child);
        if (closed.get()) child.close();
        return child;
    }

    public <A, R> CompletableFuture<R> query(QueryRef<A, R> reference, A arguments) {
        return this.<BackendQuery, R>call(
                reference.result(),
                observer -> timedStub().query(request(reference, arguments), observer));
    }

    public <A, R> CompletableFuture<R> mutate(
            MutationRef<A, R> reference, A arguments, OperationId operation) {
        return this.<BackendMutation, R>call(
                reference.result(),
                observer -> {
                    var query = request(reference, arguments);
                    var mutation =
                            BackendMutation.newBuilder()
                                    .setFunction(query.getFunction())
                                    .setArgumentsJson(query.getArgumentsJson())
                                    .setCallerJson(query.getCallerJson())
                                    .setOperationId(operation.value())
                                    .build();
                    timedStub().mutate(mutation, observer);
                });
    }

    public <A, R> BoundQuery<R> bind(QueryRef<A, R> reference, A arguments) {
        return new BoundQuery<>(this, request(reference, arguments), reference.result());
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

    private <A, R> BackendQuery request(FunctionRef<A, R> reference, A arguments) {
        var encoded = ByteString.copyFromUtf8(reference.arguments().write(arguments));
        if (encoded.size() > 1024 * 1024) throw new IllegalArgumentException("Argument size limit");
        return BackendQuery.newBuilder()
                .setFunction(reference.path())
                .setArgumentsJson(encoded)
                .setCallerJson(caller)
                .build();
    }

    private BackendGrpc.BackendStub timedStub() {
        return stub.withDeadlineAfter(deadline.toNanos(), TimeUnit.NANOSECONDS);
    }

    private <Q, R> CompletableFuture<R> call(
            JsonType<R> resultType, Consumer<ClientResponseObserver<Q, BackendResult>> start) {
        var result = new CompletableFuture<R>();
        calls.add(result);
        result.whenComplete((value, error) -> calls.remove(result));
        if (closed.get()) {
            result.cancel(false);
            return result;
        }
        try {
            start.accept(
                    new ClientResponseObserver<Q, BackendResult>() {
                        private BackendResult response;

                        public void beforeStart(ClientCallStreamObserver<Q> stream) {
                            result.whenComplete(
                                    (value, error) -> {
                                        if (result.isCancelled())
                                            stream.cancel("session scope closed", null);
                                    });
                        }

                        public void onNext(BackendResult value) {
                            response = value;
                        }

                        public void onError(Throwable error) {
                            result.completeExceptionally(error);
                        }

                        public void onCompleted() {
                            if (result.isDone()) return;
                            try {
                                if (response == null)
                                    throw new IllegalStateException("Missing backend result");
                                var encoded = response.getResultJson();
                                if (!encoded.isValidUtf8() || encoded.size() > 1024 * 1024)
                                    throw new IllegalArgumentException("Invalid backend JSON");
                                result.complete(resultType.read(encoded.toStringUtf8()));
                            } catch (RuntimeException error) {
                                result.completeExceptionally(error);
                            }
                        }
                    });
        } catch (RuntimeException error) {
            result.completeExceptionally(error);
        }
        return result;
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
