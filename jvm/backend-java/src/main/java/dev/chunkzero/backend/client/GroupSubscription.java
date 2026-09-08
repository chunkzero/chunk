package dev.chunkzero.backend.client;

import chunk.v1.BackendOuterClass.BackendUpdate;
import chunk.v1.BackendOuterClass.BackendWatch;
import dev.chunkzero.backend.api.Codecs;
import io.grpc.Status;
import io.grpc.stub.ClientCallStreamObserver;
import io.grpc.stub.ClientResponseObserver;
import java.util.ArrayList;
import java.util.List;
import java.util.Optional;
import java.util.concurrent.ScheduledFuture;
import java.util.concurrent.TimeUnit;
import java.util.function.Consumer;

/** Observer callbacks are serialized and must return promptly. */
final class GroupSubscription implements AutoCloseable {
    private final BackendSession owner;
    private final List<BoundQuery<?>> queries;
    private final BackendWatch request;
    private final Consumer<GroupState> observer;
    private boolean closed;
    private long generation;
    private GroupState.Snapshot snapshot;
    private ClientCallStreamObserver<BackendWatch> stream;
    private ScheduledFuture<?> retry;

    GroupSubscription(BackendSession owner, List<BoundQuery<?>> queries, Consumer<GroupState> observer) {
        this.owner = owner; this.queries = queries; this.observer = observer;
        request = BackendWatch.newBuilder().addAllQueries(queries.stream().map(query -> query.request).toList()).build();
    }
    synchronized void start() {
        if (closed || owner.closed.get()) { close(); return; }
        deliver(new GroupState(true, Optional.empty(), Optional.empty())); connect();
    }
    private synchronized void connect() {
        if (closed || owner.closed.get()) { close(); return; }
        long attempt = ++generation;
        owner.stub.watch(request, new ClientResponseObserver<BackendWatch, BackendUpdate>() {
            public void beforeStart(ClientCallStreamObserver<BackendWatch> call) {
                synchronized (GroupSubscription.this) {
                    if (closed || attempt != generation) call.cancel("scope closed", null); else stream = call;
                }
            }
            public void onNext(BackendUpdate update) {
                synchronized (GroupSubscription.this) {
                    if (closed || attempt != generation) return;
                    try {
                        if (update.getResultsJsonCount() != queries.size() || (update.getErrorsCount() != 0 && update.getErrorsCount() != queries.size()))
                            throw new IllegalArgumentException("Invalid query group shape");
                        if (update.getRevision() < 0 || (snapshot != null && update.getRevision() < snapshot.revision())) throw new IllegalArgumentException("Backend revision regressed");
                        int bytes = 0;
                        var results = new ArrayList<QueryResult<?>>();
                        for (int index = 0; index < queries.size(); index++) {
                            var encoded = update.getResultsJson(index);
                            bytes += encoded.size();
                            if (bytes > 1024 * 1024 || !encoded.isValidUtf8()) throw new IllegalArgumentException("Invalid backend group JSON");
                            String error = update.getErrorsCount() == 0 ? "" : update.getErrors(index);
                            results.add(error.isEmpty() ? decode(queries.get(index), update.getResultsJson(index).toStringUtf8()) : new QueryResult.Failure<>(error));
                        }
                        snapshot = new GroupState.Snapshot(update.getRevision(), queries, results);
                    } catch (RuntimeException error) { failed(attempt, Status.DATA_LOSS.withDescription(error.getMessage())); return; }
                    deliver(new GroupState(false, Optional.of(snapshot), Optional.empty()));
                }
            }
            public void onError(Throwable error) { failed(attempt, Status.fromThrowable(error)); }
            public void onCompleted() { failed(attempt, Status.UNAVAILABLE); }
        });
    }
    private static <T> QueryResult<T> decode(BoundQuery<T> query, String json) {
        return new QueryResult.Value<>(query.codec.decode(Codecs.parse(json)));
    }
    private synchronized void failed(long attempt, Status status) {
        if (closed || generation != attempt) return;
        ++generation;
        var previous = stream; stream = null;
        if (previous != null) previous.cancel("backend watch interrupted", null);
        deliver(new GroupState(true, Optional.ofNullable(snapshot), Optional.of(status.toString())));
        if (closed) return;
        if (status.getCode() == Status.Code.UNAVAILABLE || status.getCode() == Status.Code.RESOURCE_EXHAUSTED || status.getCode() == Status.Code.DEADLINE_EXCEEDED) {
            try { retry = owner.scheduler.schedule(this::connect, 500, TimeUnit.MILLISECONDS); }
            catch (RuntimeException error) { close(); throw error; }
        }
    }
    private void deliver(GroupState state) {
        try { observer.accept(state); }
        catch (RuntimeException error) { close(); throw error; }
    }
    @Override public synchronized void close() {
        if (closed) return;
        closed = true; ++generation;
        if (retry != null) retry.cancel(false);
        if (stream != null) stream.cancel("session scope closed", null);
        stream = null; owner.watches.remove(this);
    }
}
