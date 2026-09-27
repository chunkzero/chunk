package dev.chunkzero.backend.client;

import chunk.sync.v1.CoreOuterClass.Cursor;
import chunk.sync.v1.CoreOuterClass.Entry;
import chunk.sync.v1.CoreOuterClass.Error;
import chunk.sync.v1.CoreOuterClass.Position;
import chunk.sync.v1.CoreOuterClass.Update;

import io.grpc.Context;
import io.grpc.Status;
import io.grpc.stub.StreamObserver;

import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.Optional;
import java.util.concurrent.ScheduledFuture;
import java.util.concurrent.TimeUnit;
import java.util.function.Consumer;

/**
 * Keeps a group's view current from the transport's updates. A broken stream resumes after the last
 * applied position; one that ended STOPPED starts over from a snapshot. Observer callbacks run on
 * the owner's dispatcher, never on the transport's threads, and a slow observer skips to the latest
 * state. Closing interrupts a callback in progress and waits for it, so none runs once close
 * returns.
 */
final class GroupSubscription implements AutoCloseable {
    private final BackendSession owner;
    private final List<BoundQuery<?>> queries;
    private final List<Transport.Invocation> requests;
    private final Consumer<GroupState> observer;
    private boolean closed;
    private long generation;
    private Context.CancellableContext stream;
    private ScheduledFuture<?> retry;
    // The applied view, and the position and stream it resumes after.
    private GroupState.Snapshot snapshot;
    private Position position = Position.getDefaultInstance();
    private String resumable;
    // The current stream's ID and the continued parts of its pending update.
    private String current;
    private final List<Update> parts = new ArrayList<>();
    private boolean stale = true;
    // The latest state the observer has yet to see; set only while queued on the dispatcher.
    private GroupState pending;

    GroupSubscription(
            BackendSession owner, List<BoundQuery<?>> queries, Consumer<GroupState> observer) {
        this.owner = owner;
        this.queries = queries;
        this.observer = observer;
        requests = queries.stream().map(query -> query.request).toList();
    }

    /** Queues the initial stale state, then connects. */
    synchronized void start() {
        if (closed) return;
        publish(new GroupState(true, Optional.empty(), Optional.empty()));
        connect();
    }

    private synchronized void connect() {
        if (closed || owner.closed.get()) {
            shut();
            return;
        }
        long attempt = ++generation;
        current = null;
        var after =
                resumable == null
                        ? null
                        : Cursor.newBuilder().setStream(resumable).setPosition(position).build();
        stream = Context.ROOT.withCancellation();
        stream.run(
                () ->
                        owner.transport.watch(
                                requests,
                                owner.identity,
                                after,
                                new StreamObserver<Update>() {
                                    public void onNext(Update update) {
                                        received(attempt, update);
                                    }

                                    public void onError(Throwable error) {
                                        var status = Status.fromThrowable(error);
                                        failed(attempt, status, retryable(status), false);
                                    }

                                    public void onCompleted() {
                                        failed(
                                                attempt,
                                                Status.UNAVAILABLE.withDescription(
                                                        "backend watch ended"),
                                                true,
                                                false);
                                    }
                                }));
    }

    private synchronized void received(long attempt, Update update) {
        if (closed || attempt != generation) return;
        if (update.hasError()) {
            var code = update.getError().getCode();
            boolean restart = code == Error.Code.CODE_STOPPED;
            boolean reconnect =
                    restart
                            || code == Error.Code.CODE_UNAVAILABLE
                            || code == Error.Code.CODE_OVERLOADED;
            failed(attempt, CoreTransport.status(update.getError()), reconnect, restart);
            return;
        }
        if (!update.getStream().isEmpty()) current = update.getStream();
        parts.add(update);
        if (update.getContinued()) return;
        var sequence = List.copyOf(parts);
        parts.clear();
        var first = sequence.getFirst();
        boolean changed =
                first.getSnapshot()
                        || sequence.stream()
                                .anyMatch(
                                        part ->
                                                part.getUpsertsCount() != 0
                                                        || part.getRemovedCount() != 0);
        GroupState.Snapshot next;
        try {
            next = apply(sequence);
        } catch (RuntimeException error) {
            failed(attempt, Status.DATA_LOSS.withDescription(error.getMessage()), false, false);
            return;
        }
        position = first.getPosition();
        resumable = current;
        if (!changed && !stale) return;
        snapshot = next;
        stale = false;
        publish(new GroupState(false, Optional.of(snapshot), Optional.empty()));
    }

    /** The view after a complete update, which a snapshot replaces and changes amend. */
    private GroupState.Snapshot apply(List<Update> sequence) {
        var first = sequence.getFirst();
        List<QueryResult<?>> results;
        if (first.getSnapshot()) {
            results = new ArrayList<>(Collections.nCopies(queries.size(), null));
        } else if (snapshot == null) {
            throw new IllegalArgumentException("Backend changes arrived before a snapshot");
        } else if (before(first.getPosition(), position)) {
            throw new IllegalArgumentException("Backend position regressed");
        } else {
            results = new ArrayList<>(snapshot.results());
        }
        for (var update : sequence) {
            for (var entry : update.getUpsertsList()) {
                int index = index(entry.getKey());
                results.set(index, result(queries.get(index), entry));
            }
            for (var key : update.getRemovedList()) results.set(index(key), null);
        }
        if (results.contains(null))
            throw new IllegalArgumentException("Backend group is missing a query");
        return new GroupState.Snapshot(first.getPosition().getRevision(), queries, results);
    }

    private int index(String key) {
        int index = Integer.parseInt(key);
        if (index < 0 || index >= queries.size() || !key.equals(Integer.toString(index)))
            throw new IllegalArgumentException("Unknown backend query key");
        return index;
    }

    private static <T> QueryResult<T> result(BoundQuery<T> query, Entry entry) {
        return switch (entry.getStateCase()) {
            case VALUE -> {
                if (!entry.getValue().isValidUtf8())
                    throw new IllegalArgumentException("Invalid backend group JSON");
                yield new QueryResult.Value<>(query.type.read(entry.getValue().toStringUtf8()));
            }
            case ERROR -> new QueryResult.Failure<>(entry.getError().getMessage());
            default -> throw new IllegalArgumentException("Backend entry has no state");
        };
    }

    private static boolean before(Position position, Position last) {
        return position.getEpoch() < last.getEpoch()
                || (position.getEpoch() == last.getEpoch()
                        && position.getRevision() < last.getRevision());
    }

    private static boolean retryable(Status status) {
        return status.getCode() == Status.Code.UNAVAILABLE
                || status.getCode() == Status.Code.RESOURCE_EXHAUSTED
                || status.getCode() == Status.Code.DEADLINE_EXCEEDED;
    }

    private synchronized void failed(
            long attempt, Status status, boolean reconnect, boolean restart) {
        if (closed || generation != attempt) return;
        ++generation;
        parts.clear();
        if (restart) resumable = null;
        if (stream != null) stream.cancel(null);
        stream = null;
        stale = true;
        publish(
                new GroupState(
                        true, Optional.ofNullable(snapshot), Optional.of(status.toString())));
        if (!reconnect) return;
        try {
            retry = owner.scheduler.schedule(this::connect, 500, TimeUnit.MILLISECONDS);
        } catch (RuntimeException error) {
            shut();
        }
    }

    /** Queues {@code state} for the observer, replacing any state it has yet to see. */
    private void publish(GroupState state) {
        boolean queued = pending != null;
        pending = state;
        if (!queued) owner.dispatcher.schedule(this);
    }

    /** Runs the observer with the latest state unless closed; called on the owner's dispatcher. */
    void deliver() {
        GroupState state;
        synchronized (this) {
            state = pending;
            pending = null;
            if (state == null || closed) return;
        }
        try {
            observer.accept(state);
        } catch (RuntimeException error) {
            close();
            var thread = Thread.currentThread();
            thread.getUncaughtExceptionHandler().uncaughtException(thread, error);
        }
    }

    /**
     * Stops later callbacks, then interrupts one in progress and waits for it, unless called from a
     * callback or during a session's close.
     */
    @Override
    public void close() {
        shut();
        if (owner.mayWait()) owner.dispatcher.finish(this);
    }

    /** Stops the stream and later callbacks. */
    synchronized void shut() {
        if (closed) return;
        closed = true;
        ++generation;
        pending = null;
        if (retry != null) retry.cancel(false);
        if (stream != null) stream.cancel(null);
        stream = null;
        owner.watches.remove(this);
    }
}
