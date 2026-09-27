package dev.chunkzero.backend.client;

import static org.junit.jupiter.api.Assertions.*;

import chunk.sync.v1.CoreGrpc;
import chunk.sync.v1.CoreOuterClass.*;
import chunk.sync.v1.CoreOuterClass.Error;

import com.google.protobuf.ByteString;

import dev.chunkzero.backend.api.*;
import dev.chunkzero.generated.BackendClient;
import dev.chunkzero.generated.BackendTypes;

import io.grpc.Context;
import io.grpc.ManagedChannel;
import io.grpc.ManagedChannelBuilder;
import io.grpc.Metadata;
import io.grpc.Server;
import io.grpc.ServerBuilder;
import io.grpc.ServerCall;
import io.grpc.ServerCallHandler;
import io.grpc.ServerInterceptor;
import io.grpc.Status;
import io.grpc.stub.ServerCallStreamObserver;
import io.grpc.stub.StreamObserver;

import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

import tools.jackson.core.type.TypeReference;

import java.time.Duration;
import java.util.List;
import java.util.Optional;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.Executors;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.locks.LockSupport;
import java.util.function.Consumer;

class BackendSessionTest {
    private static final String CREDENTIAL = "test-credential-with-at-least-32-bytes";
    private static final JsonType<Void> NULL =
            JsonType.of(new TypeReference<Void>() {}, BackendValues::checkNull);
    private static final JsonType<Long> INTEGER =
            JsonType.of(new TypeReference<Long>() {}, BackendValues::checkInteger);
    private static final QueryRef<Long, Long> READ =
            new QueryRef<>("shared/read", INTEGER, INTEGER);
    private final Fixture fixture = new Fixture();
    private Server server;
    private ManagedChannel channel;
    private ScheduledExecutorService scheduler;
    private BackendSession session;

    @BeforeEach
    void start() throws Exception {
        server =
                ServerBuilder.forPort(0)
                        .intercept(
                                new ServerInterceptor() {
                                    @Override
                                    public <Q, R> ServerCall.Listener<Q> interceptCall(
                                            ServerCall<Q, R> call,
                                            Metadata headers,
                                            ServerCallHandler<Q, R> next) {
                                        assertEquals(
                                                "Bearer " + CREDENTIAL,
                                                headers.get(
                                                        Metadata.Key.of(
                                                                "authorization",
                                                                Metadata.ASCII_STRING_MARSHALLER)));
                                        return next.startCall(call, headers);
                                    }
                                })
                        .addService(fixture)
                        .build()
                        .start();
        channel =
                ManagedChannelBuilder.forAddress("127.0.0.1", server.getPort())
                        .usePlaintext()
                        .build();
        scheduler = Executors.newSingleThreadScheduledExecutor();
        session = create(Duration.ofSeconds(5));
    }

    private BackendSession create(Duration deadline) {
        return BackendSession.overCore(
                channel,
                CREDENTIAL,
                "immutable-build",
                new SessionIdentity(
                        new SessionId("s1"), "duels", Optional.of(new PlayerId("trusted"))),
                scheduler,
                deadline);
    }

    @AfterEach
    void stop() throws Exception {
        session.close();
        channel.shutdownNow().awaitTermination(2, TimeUnit.SECONDS);
        server.shutdownNow().awaitTermination(2, TimeUnit.SECONDS);
        scheduler.shutdownNow();
        assertTrue(scheduler.awaitTermination(2, TimeUnit.SECONDS));
    }

    @Test
    void generatedJavaCallRecoversLostReplyWithoutChangingIdentity() throws Exception {
        var client = new BackendClient(session);
        var arguments =
                new BackendTypes.Shared.Profile.RecordArgs(
                        1L,
                        new BackendTypes.Ids.Profiles("profiles:p1"),
                        List.of(),
                        null,
                        new PlayerId("spoof"),
                        new BackendTypes.Shared.Profile.RecordArgs.State.Ready());
        var operation = OperationId.create();
        assertThrows(
                ExecutionException.class,
                () ->
                        client.shared()
                                .profile()
                                .record_(arguments, operation)
                                .get(2, TimeUnit.SECONDS));
        var result =
                client.shared().profile().record_(arguments, operation).get(2, TimeUnit.SECONDS);
        assertTrue(result.ok());
        assertEquals(new SessionId("s1"), result.session());
        assertEquals(1, fixture.saved.size());
        assertEquals(2, fixture.calls.size());
        assertEquals(fixture.calls.get(0), fixture.calls.get(1));
        var request = fixture.calls.get(0);
        assertEquals(operation.value(), request.getOperationId());
        assertEquals("immutable-build", request.getDeployment());
        assertEquals(
                Caller.newBuilder().setSession("s1").setPlayer("trusted").build(),
                request.getCaller());
        assertEquals(
                "spoof",
                BackendJson.mapper()
                        .readTree(request.getArguments().toStringUtf8())
                        .get("player")
                        .asString());
        assertTrue(fixture.deadlineObserved);
        assertEquals(
                3L,
                client.shared()
                        .profile()
                        .read(new BackendTypes.Shared.Profile.ReadArgs(new PlayerId("spoof")))
                        .get(2, TimeUnit.SECONDS));
    }

    @Test
    void emptyArgumentOverloadsRetainTypedPayloadsRetryIdsAndFullWatchState() throws Exception {
        var profile = new BackendClient(session).shared().profile();
        assertEquals(3L, profile.total().get(2, TimeUnit.SECONDS));
        assertEquals("shared/profile/total", fixture.calls.getFirst().getMethod());
        assertEquals("{}", fixture.calls.getFirst().getArguments().toStringUtf8());

        var operation = OperationId.create();
        var error =
                assertThrows(
                        ExecutionException.class,
                        () -> profile.reward(operation).get(2, TimeUnit.SECONDS));
        assertEquals(Status.Code.UNAVAILABLE, Status.fromThrowable(error.getCause()).getCode());
        var recovered =
                profile.reward(new BackendTypes.Shared.Profile.RewardArgs(), operation)
                        .get(2, TimeUnit.SECONDS);
        assertTrue(recovered.ok());
        assertEquals(fixture.calls.get(1), fixture.calls.get(2));
        assertEquals("{}", fixture.calls.get(1).getArguments().toStringUtf8());

        var states = new LinkedBlockingQueue<WatchState<Long>>();
        Watch watch;
        try (var subscription = profile.watchTotal(states::add)) {
            assertNotNull(subscription);
            var initial = states.poll(2, TimeUnit.SECONDS);
            assertNotNull(initial);
            assertTrue(initial.stale());
            assertTrue(initial.snapshot().isEmpty());
            watch = fixture.watches.poll(2, TimeUnit.SECONDS);
            assertNotNull(watch);
            assertEquals("queries", watch.request().getTopic());
            assertEquals("immutable-build", watch.request().getDeployment());
            assertEquals(
                    "{\"0\":{\"function\":\"shared/profile/total\",\"arguments\":{}}}",
                    watch.request().getArguments().toStringUtf8());
            assertFalse(watch.request().hasAfter());
            watch.response().onNext(snapshot(1, value("0", "3")));
            var fresh = states.poll(2, TimeUnit.SECONDS);
            assertNotNull(fresh);
            assertFalse(fresh.stale());
            assertTrue(fresh.error().isEmpty());
            assertEquals(1L, fresh.snapshot().orElseThrow().revision());
            assertEquals(3L, fresh.snapshot().orElseThrow().result().valueOrThrow());
        }
        assertTrue(watch.cancelled().await(2, TimeUnit.SECONDS));
    }

    @Test
    void unaryResultWaitsForFinalStatusAndRemainsSessionOwned() throws Exception {
        var reference = new QueryRef<Void, Long>("shared/partial", NULL, INTEGER);
        var result = session.query(reference, null);
        var response = fixture.partial.poll(2, TimeUnit.SECONDS);
        assertNotNull(response);
        response.onError(Status.UNAVAILABLE.asRuntimeException());
        var error = assertThrows(ExecutionException.class, () -> result.get(2, TimeUnit.SECONDS));
        assertEquals(Status.Code.UNAVAILABLE, Status.fromThrowable(error.getCause()).getCode());

        var pending = session.query(reference, null);
        assertNotNull(fixture.partial.poll(2, TimeUnit.SECONDS));
        session.close();
        assertTrue(pending.isCancelled());
    }

    @Test
    void groupedWatchSignalsStaleThenReplacesOneConsistentSnapshot() throws Exception {
        var first = session.bind(READ, 1L);
        var second = session.bind(READ, 2L);
        var states = new LinkedBlockingQueue<GroupState>();
        try (var subscription = session.watchGroup(List.of(first, second), states::add)) {
            assertNotNull(subscription);
            var initial = states.poll(2, TimeUnit.SECONDS);
            assertNotNull(initial);
            assertTrue(initial.stale());
            assertTrue(initial.snapshot().isEmpty());
            var watch = fixture.watches.poll(2, TimeUnit.SECONDS);
            assertNotNull(watch);
            watch.response()
                    .onNext(
                            snapshot(
                                    1,
                                    value("0", "1"),
                                    Entry.newBuilder()
                                            .setKey("1")
                                            .setError(
                                                    Error.newBuilder()
                                                            .setCode(Error.Code.CODE_APPLICATION)
                                                            .setMessage("missing document"))
                                            .build()));
            var fresh = states.poll(2, TimeUnit.SECONDS);
            assertNotNull(fresh);
            assertFalse(fresh.stale());
            assertEquals(1, fresh.snapshot().orElseThrow().revision());
            assertEquals(1L, fresh.snapshot().orElseThrow().result(first).valueOrThrow());
            assertEquals(
                    new QueryResult.Failure<Long>("missing document"),
                    fresh.snapshot().orElseThrow().result(second));
            watch.response().onError(Status.UNAVAILABLE.asRuntimeException());
            var stale = states.poll(2, TimeUnit.SECONDS);
            assertNotNull(stale);
            assertTrue(stale.stale());
            assertEquals(fresh.snapshot(), stale.snapshot());
            var next = fixture.watches.poll(3, TimeUnit.SECONDS);
            assertNotNull(next);
            next.response().onNext(snapshot(2, value("0", "2"), value("1", "3")));
            var recovered = states.poll(2, TimeUnit.SECONDS);
            assertNotNull(recovered);
            assertFalse(recovered.stale());
            assertEquals(2, recovered.snapshot().orElseThrow().revision());
            assertEquals(2L, recovered.snapshot().orElseThrow().result(first).valueOrThrow());
            assertEquals(3L, recovered.snapshot().orElseThrow().result(second).valueOrThrow());
        }
    }

    @Test
    void brokenWatchResumesAfterItsLastAppliedPosition() throws Exception {
        var states = new LinkedBlockingQueue<GroupState>();
        try (var subscription = session.watchGroup(List.of(session.bind(READ, 1L)), states::add)) {
            assertNotNull(subscription);
            assertTrue(states.poll(2, TimeUnit.SECONDS).stale());
            var watch = fixture.watches.poll(2, TimeUnit.SECONDS);
            watch.response()
                    .onNext(snapshot(1, value("0", "1")).toBuilder().setStream("a").build());
            assertEquals(1L, value(states.poll(2, TimeUnit.SECONDS), 1));
            // A position-only update moves the resume point without a new state, and a continued
            // update the break cuts short is discarded.
            watch.response().onNext(Update.newBuilder().setPosition(at(2)).build());
            watch.response()
                    .onNext(
                            Update.newBuilder()
                                    .setPosition(at(3))
                                    .addUpserts(value("0", "3"))
                                    .setContinued(true)
                                    .build());
            watch.response().onError(Status.UNAVAILABLE.asRuntimeException());
            var stale = states.poll(2, TimeUnit.SECONDS);
            assertTrue(stale.stale());
            assertEquals(1, stale.snapshot().orElseThrow().revision());
            assertEquals(1L, stale.snapshot().orElseThrow().results().getFirst().valueOrThrow());

            var resumed = fixture.watches.poll(3, TimeUnit.SECONDS);
            assertEquals(
                    Cursor.newBuilder().setStream("a").setPosition(at(2)).build(),
                    resumed.request().getAfter());
            resumed.response()
                    .onNext(Update.newBuilder().setPosition(at(3)).setStream("b").build());
            assertEquals(1L, value(states.poll(2, TimeUnit.SECONDS), 3));
            resumed.response()
                    .onNext(
                            Update.newBuilder()
                                    .setPosition(at(4))
                                    .addUpserts(value("0", "4"))
                                    .setContinued(true)
                                    .build());
            resumed.response().onNext(Update.newBuilder().setPosition(at(4)).build());
            assertEquals(4L, value(states.poll(2, TimeUnit.SECONDS), 4));

            resumed.response()
                    .onNext(
                            Update.newBuilder()
                                    .setError(
                                            Error.newBuilder()
                                                    .setCode(Error.Code.CODE_STOPPED)
                                                    .setMessage("superseded"))
                                    .build());
            var stopped = states.poll(2, TimeUnit.SECONDS);
            assertTrue(stopped.stale());
            assertTrue(stopped.error().orElseThrow().contains("superseded"));
            var restarted = fixture.watches.poll(3, TimeUnit.SECONDS);
            assertFalse(restarted.request().hasAfter());
        }
    }

    @Test
    void slowObserverDoesNotHoldBackAnotherWatchOnTheChannel() throws Exception {
        // Callbacks run on the channel's transport thread, which an observer blocking there would
        // hold for every stream.
        var direct =
                ManagedChannelBuilder.forAddress("127.0.0.1", server.getPort())
                        .usePlaintext()
                        .directExecutor()
                        .build();
        var release = new CountDownLatch(1);
        var blocked = new CountDownLatch(1);
        var interrupted = new CountDownLatch(1);
        var slowStates = new LinkedBlockingQueue<GroupState>();
        try (var shared =
                BackendSession.overCore(
                        direct,
                        CREDENTIAL,
                        "immutable-build",
                        new SessionIdentity(new SessionId("s1"), "duels", Optional.empty()),
                        scheduler,
                        Duration.ofSeconds(5))) {
            var slow =
                    shared.watchGroup(
                            List.of(shared.bind(READ, 1L)),
                            state -> {
                                slowStates.add(state);
                                if (state.stale()) return;
                                boolean last = state.snapshot().orElseThrow().revision() == 5;
                                try {
                                    (last ? blocked : release).await();
                                } catch (InterruptedException error) {
                                    interrupted.countDown();
                                }
                            });
            assertNotNull(slow);
            var slowWatch = fixture.watches.poll(2, TimeUnit.SECONDS);
            assertNotNull(slowWatch);
            slowWatch.response().onNext(snapshot(1, value("0", "1")));
            // Larger than gRPC's default 4 MiB inbound message limit.
            var large = "\"" + "x".repeat(5 * 1024 * 1024) + "\"";
            for (int revision = 2; revision <= 4; revision++)
                slowWatch
                        .response()
                        .onNext(
                                Update.newBuilder()
                                        .setPosition(at(revision))
                                        .addUpserts(
                                                Entry.newBuilder()
                                                        .setKey("0")
                                                        .setError(
                                                                Error.newBuilder()
                                                                        .setMessage(large)))
                                        .build());

            var fastStates = new LinkedBlockingQueue<GroupState>();
            try (var fast = shared.watchGroup(List.of(shared.bind(READ, 2L)), fastStates::add)) {
                assertNotNull(fast);
                assertTrue(fastStates.poll(2, TimeUnit.SECONDS).stale());
                var fastWatch = fixture.watches.poll(2, TimeUnit.SECONDS);
                assertNotNull(fastWatch);
                fastWatch.response().onNext(snapshot(1, value("0", "2")));
                assertEquals(2L, value(fastStates.poll(2, TimeUnit.SECONDS), 1));
            } finally {
                release.countDown();
            }
            var deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
            long revision = 0;
            while (revision != 4 && System.nanoTime() < deadline) {
                var state = slowStates.poll(100, TimeUnit.MILLISECONDS);
                if (state != null && !state.stale())
                    revision = state.snapshot().orElseThrow().revision();
            }
            assertEquals(4, revision);

            // Closing interrupts an observer blocked in its callback and waits for it to return.
            slowWatch.response().onNext(snapshot(5, value("0", "5")));
            assertEquals(5L, value(slowStates.poll(2, TimeUnit.SECONDS), 5));
            slow.close();
            assertEquals(0, interrupted.getCount());
        } finally {
            direct.shutdownNow().awaitTermination(2, TimeUnit.SECONDS);
        }
    }

    @Test
    void noCallbackRunsOnceCloseReturns() throws Exception {
        var late = new AtomicInteger();
        for (int round = 0; round < 20; round++) {
            var closed = new AtomicBoolean();
            Consumer<GroupState> observer =
                    state -> {
                        LockSupport.parkNanos(TimeUnit.MICROSECONDS.toNanos(200));
                        if (closed.get()) late.incrementAndGet();
                    };

            // Queued delivery racing close.
            var queued = session.forPlayer(new PlayerId("queued-" + round));
            queued.watchGroup(List.of(queued.bind(READ, 1L)), observer);
            var watch = watchFor("queued-" + round);
            var sender =
                    Thread.startVirtualThread(
                            () -> {
                                for (int revision = 1; revision <= 20; revision++) {
                                    try {
                                        watch.response()
                                                .onNext(snapshot(revision, value("0", "1")));
                                    } catch (RuntimeException cancelled) {
                                        return;
                                    }
                                }
                            });
            LockSupport.parkNanos(TimeUnit.MICROSECONDS.toNanos(round * 50L));
            queued.close();
            closed.set(true);
            sender.join();

            // Initial delivery racing close.
            closed.set(false);
            var initial = session.forPlayer(new PlayerId("initial-" + round));
            var starter =
                    Thread.startVirtualThread(
                            () -> initial.watchGroup(List.of(initial.bind(READ, 1L)), observer));
            initial.close();
            closed.set(true);
            starter.join();
        }
        Thread.sleep(50);
        assertEquals(0, late.get());
    }

    private Watch watchFor(String player) throws InterruptedException {
        while (true) {
            var watch = fixture.watches.poll(2, TimeUnit.SECONDS);
            assertNotNull(watch);
            if (watch.request().getCaller().getPlayer().equals(player)) return watch;
        }
    }

    @Test
    void successfulNullWatchResultIsAValue() throws Exception {
        var states = new LinkedBlockingQueue<WatchState<Void>>();
        var reference = new QueryRef<Void, Void>("shared/null", NULL, NULL);
        try (var subscription = session.watch(reference, null, states::add)) {
            assertNotNull(subscription);
            assertTrue(states.poll(2, TimeUnit.SECONDS).stale());
            fixture.watches
                    .poll(2, TimeUnit.SECONDS)
                    .response()
                    .onNext(snapshot(1, value("0", "null")));
            var fresh = states.poll(2, TimeUnit.SECONDS);
            assertNotNull(fresh);
            assertFalse(fresh.stale());
            var result = fresh.snapshot().orElseThrow().result();
            assertInstanceOf(QueryResult.Value.class, result);
            assertNull(result.valueOrThrow());
        }
    }

    @Test
    void playerDepartureCancelsItsCallsAndDeadlinesReachTheServer() throws Exception {
        var a = session.forPlayer(new PlayerId("a"));
        var b = session.forPlayer(new PlayerId("b"));
        var hang = new QueryRef<Void, Long>("shared/hang", NULL, INTEGER);
        var waiting = new BackendClient(a).shared().hang(null);
        assertTrue(fixture.hanging.await(2, TimeUnit.SECONDS));
        a.close();
        assertTrue(waiting.isCancelled());
        assertTrue(fixture.cancelled.await(2, TimeUnit.SECONDS));
        var read = new QueryRef<Void, Long>("shared/read", NULL, INTEGER);
        assertEquals(3L, b.query(read, null).get(2, TimeUnit.SECONDS));
        assertEquals("b", fixture.calls.getLast().getCaller().getPlayer());
        try (var shortDeadline = create(Duration.ofMillis(50))) {
            var error =
                    assertThrows(
                            ExecutionException.class,
                            () -> shortDeadline.query(hang, null).get(2, TimeUnit.SECONDS));
            assertEquals(
                    Status.Code.DEADLINE_EXCEEDED,
                    Status.fromThrowable(error.getCause()).getCode());
        }
        session.close();
        assertTrue(b.query(read, null).isCancelled());
    }

    private static Position at(long revision) {
        return Position.newBuilder().setEpoch(1).setRevision(revision).build();
    }

    private static Entry value(String key, String json) {
        return Entry.newBuilder().setKey(key).setValue(ByteString.copyFromUtf8(json)).build();
    }

    private static Update snapshot(long revision, Entry... entries) {
        return Update.newBuilder()
                .setPosition(at(revision))
                .setSnapshot(true)
                .setStream("stream")
                .addAllUpserts(List.of(entries))
                .build();
    }

    /** The single query's value in a fresh state at {@code revision}. */
    private static Object value(GroupState state, long revision) {
        assertNotNull(state);
        assertFalse(state.stale());
        var snapshot = state.snapshot().orElseThrow();
        assertEquals(revision, snapshot.revision());
        return snapshot.results().getFirst().valueOrThrow();
    }

    private record Watch(
            SubscribeRequest request,
            ServerCallStreamObserver<Update> response,
            CountDownLatch cancelled) {}

    private static final class Fixture extends CoreGrpc.CoreImplBase {
        final ConcurrentHashMap<String, CallResponse> saved = new ConcurrentHashMap<>();
        final CopyOnWriteArrayList<CallRequest> calls = new CopyOnWriteArrayList<>();
        final CountDownLatch hanging = new CountDownLatch(1);
        final CountDownLatch cancelled = new CountDownLatch(1);
        final LinkedBlockingQueue<StreamObserver<CallResponse>> partial =
                new LinkedBlockingQueue<>();
        final LinkedBlockingQueue<Watch> watches = new LinkedBlockingQueue<>();
        volatile boolean deadlineObserved;

        @Override
        public void call(CallRequest request, StreamObserver<CallResponse> response) {
            deadlineObserved = Context.current().getDeadline() != null;
            calls.add(request);
            switch (request.getMethod()) {
                case "shared/partial" -> {
                    response.onNext(result("3"));
                    partial.add(response);
                }
                case "shared/hang" -> {
                    ((ServerCallStreamObserver<CallResponse>) response)
                            .setOnCancelHandler(cancelled::countDown);
                    hanging.countDown();
                }
                default -> {
                    var result =
                            request.getOperationId().isEmpty()
                                    ? result("3")
                                    : result("{\"ok\":true,\"session\":\"s1\"}");
                    if (!request.getOperationId().isEmpty()
                            && saved.putIfAbsent(request.getOperationId(), result) == null) {
                        if (request.getMethod().equals("shared/profile/reward")) {
                            var error =
                                    Error.newBuilder()
                                            .setCode(Error.Code.CODE_UNAVAILABLE)
                                            .setMessage("storage is busy");
                            response.onNext(CallResponse.newBuilder().setError(error).build());
                            response.onCompleted();
                        } else response.onError(Status.UNAVAILABLE.asRuntimeException());
                        return;
                    }
                    response.onNext(result);
                    response.onCompleted();
                }
            }
        }

        @Override
        public void subscribe(SubscribeRequest request, StreamObserver<Update> response) {
            var observer = (ServerCallStreamObserver<Update>) response;
            var cancelled = new CountDownLatch(1);
            observer.setOnCancelHandler(cancelled::countDown);
            watches.add(new Watch(request, observer, cancelled));
        }

        private static CallResponse result(String json) {
            return CallResponse.newBuilder()
                    .setPosition(at(1))
                    .setResult(ByteString.copyFromUtf8(json))
                    .build();
        }
    }
}
