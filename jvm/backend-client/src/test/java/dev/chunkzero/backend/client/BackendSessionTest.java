package dev.chunkzero.backend.client;

import static org.junit.jupiter.api.Assertions.*;

import chunk.v1.BackendGrpc;
import chunk.v1.BackendOuterClass.*;

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
import java.util.concurrent.atomic.AtomicInteger;

class BackendSessionTest {
    private static final JsonType<Void> NULL =
            JsonType.of(new TypeReference<Void>() {}, BackendValues::checkNull);
    private static final JsonType<Long> INTEGER =
            JsonType.of(new TypeReference<Long>() {}, BackendValues::checkInteger);
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
                                                "local",
                                                headers.get(
                                                        Metadata.Key.of(
                                                                "x-chunk-environment",
                                                                Metadata.ASCII_STRING_MARSHALLER)));
                                        assertEquals(
                                                "immutable-build",
                                                headers.get(
                                                        Metadata.Key.of(
                                                                "x-chunk-deployment",
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
        return new BackendSession(
                channel,
                "test-credential-with-at-least-32-bytes",
                "local",
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
        assertEquals(
                "trusted",
                BackendJson.mapper()
                        .readTree(request.getCallerJson().toStringUtf8())
                        .get("player")
                        .asString());
        assertEquals(
                "spoof",
                BackendJson.mapper()
                        .readTree(request.getArgumentsJson().toStringUtf8())
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
        assertEquals("shared/profile/total", fixture.calls.getFirst().getFunction());
        assertEquals("{}", fixture.calls.getFirst().getArgumentsJson().toStringUtf8());

        var operation = OperationId.create();
        assertThrows(
                ExecutionException.class, () -> profile.reward(operation).get(2, TimeUnit.SECONDS));
        var recovered =
                profile.reward(new BackendTypes.Shared.Profile.RewardArgs(), operation)
                        .get(2, TimeUnit.SECONDS);
        assertTrue(recovered.ok());
        assertEquals(fixture.calls.get(1), fixture.calls.get(2));
        assertEquals("{}", fixture.calls.get(1).getArgumentsJson().toStringUtf8());

        var states = new LinkedBlockingQueue<WatchState<Long>>();
        try (var watch = profile.watchTotal(states::add)) {
            assertNotNull(watch);
            var initial = states.poll(2, TimeUnit.SECONDS);
            assertNotNull(initial);
            assertTrue(initial.stale());
            assertTrue(initial.snapshot().isEmpty());
            var fresh = states.poll(2, TimeUnit.SECONDS);
            assertNotNull(fresh);
            assertFalse(fresh.stale());
            assertTrue(fresh.error().isEmpty());
            assertEquals(1L, fresh.snapshot().orElseThrow().revision());
            assertEquals(3L, fresh.snapshot().orElseThrow().result().valueOrThrow());
            var request = fixture.watchRequests.poll(2, TimeUnit.SECONDS);
            assertNotNull(request);
            assertEquals("shared/profile/total", request.getQueries(0).getFunction());
            assertEquals("{}", request.getQueries(0).getArgumentsJson().toStringUtf8());
        }
        assertTrue(fixture.watchCancelled.await(2, TimeUnit.SECONDS));
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
        var reference = new QueryRef<Long, Long>("shared/read", INTEGER, INTEGER);
        var first = session.bind(reference, 1L);
        var second = session.bind(reference, 2L);
        var states = new LinkedBlockingQueue<GroupState>();
        try (var watch = session.watchGroup(List.of(first, second), states::add)) {
            assertNotNull(watch);
            var initial = states.poll(2, TimeUnit.SECONDS);
            assertNotNull(initial);
            assertTrue(initial.stale());
            assertTrue(initial.snapshot().isEmpty());
            var fresh = states.poll(2, TimeUnit.SECONDS);
            assertNotNull(fresh);
            assertFalse(fresh.stale());
            assertEquals(1, fresh.snapshot().orElseThrow().revision());
            assertEquals(1L, fresh.snapshot().orElseThrow().result(first).valueOrThrow());
            assertInstanceOf(
                    QueryResult.Failure.class, fresh.snapshot().orElseThrow().result(second));
            fixture.watch.onError(Status.UNAVAILABLE.asRuntimeException());
            var stale = states.poll(2, TimeUnit.SECONDS);
            assertNotNull(stale);
            assertTrue(stale.stale());
            assertEquals(fresh.snapshot(), stale.snapshot());
            var recovered = states.poll(3, TimeUnit.SECONDS);
            assertNotNull(recovered);
            assertFalse(recovered.stale());
            assertEquals(2, recovered.snapshot().orElseThrow().revision());
            assertEquals(2L, recovered.snapshot().orElseThrow().result(first).valueOrThrow());
            assertEquals(3L, recovered.snapshot().orElseThrow().result(second).valueOrThrow());
        }
    }

    @Test
    void successfulNullWatchResultIsAValue() throws Exception {
        var states = new LinkedBlockingQueue<WatchState<Void>>();
        var reference = new QueryRef<Void, Void>("shared/null", NULL, NULL);
        try (var watch = session.watch(reference, null, states::add)) {
            assertNotNull(watch);
            assertTrue(states.poll(2, TimeUnit.SECONDS).stale());
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

    private static final class Fixture extends BackendGrpc.BackendImplBase {
        final ConcurrentHashMap<String, BackendResult> saved = new ConcurrentHashMap<>();
        final CopyOnWriteArrayList<BackendMutation> calls = new CopyOnWriteArrayList<>();
        final AtomicInteger watches = new AtomicInteger();
        final CountDownLatch hanging = new CountDownLatch(1);
        final CountDownLatch cancelled = new CountDownLatch(1);
        final CountDownLatch watchCancelled = new CountDownLatch(1);
        final LinkedBlockingQueue<BackendWatchGroup> watchRequests = new LinkedBlockingQueue<>();
        final LinkedBlockingQueue<StreamObserver<BackendResult>> partial =
                new LinkedBlockingQueue<>();
        volatile StreamObserver<BackendUpdate> watch;
        volatile boolean deadlineObserved;

        @Override
        public void query(BackendQuery request, StreamObserver<BackendResult> response) {
            deadlineObserved = Context.current().getDeadline() != null;
            calls.add(
                    BackendMutation.newBuilder()
                            .setFunction(request.getFunction())
                            .setArgumentsJson(request.getArgumentsJson())
                            .setCallerJson(request.getCallerJson())
                            .build());
            if (request.getFunction().equals("shared/partial")) {
                response.onNext(
                        BackendResult.newBuilder()
                                .setRevision(1)
                                .setResultJson(ByteString.copyFromUtf8("3"))
                                .build());
                partial.add(response);
                return;
            }
            if (request.getFunction().equals("shared/hang")) {
                ((ServerCallStreamObserver<BackendResult>) response)
                        .setOnCancelHandler(cancelled::countDown);
                hanging.countDown();
                return;
            }
            {
                response.onNext(
                        BackendResult.newBuilder()
                                .setRevision(1)
                                .setResultJson(ByteString.copyFromUtf8("3"))
                                .build());
                response.onCompleted();
                return;
            }
        }

        @Override
        public void mutate(BackendMutation request, StreamObserver<BackendResult> response) {
            deadlineObserved = Context.current().getDeadline() != null;
            calls.add(request);
            var result =
                    BackendResult.newBuilder()
                            .setRevision(1)
                            .setResultJson(
                                    ByteString.copyFromUtf8("{\"ok\":true,\"session\":\"s1\"}"))
                            .build();
            if (saved.putIfAbsent(request.getOperationId(), result) == null)
                response.onError(Status.UNAVAILABLE.asRuntimeException());
            else {
                response.onNext(saved.get(request.getOperationId()));
                response.onCompleted();
            }
        }

        @Override
        public void watchGroup(BackendWatchGroup request, StreamObserver<BackendUpdate> response) {
            if (request.getQueriesCount() == 1) {
                watchRequests.add(request);
                ((ServerCallStreamObserver<BackendUpdate>) response)
                        .setOnCancelHandler(watchCancelled::countDown);
                response.onNext(
                        BackendUpdate.newBuilder()
                                .setRevision(1)
                                .addResultsJson(
                                        ByteString.copyFromUtf8(
                                                request.getQueries(0)
                                                                .getFunction()
                                                                .equals("shared/null")
                                                        ? "null"
                                                        : "3"))
                                .addErrors("")
                                .build());
                return;
            }
            int attempt = watches.incrementAndGet();
            watch = response;
            response.onNext(
                    BackendUpdate.newBuilder()
                            .setRevision(attempt)
                            .addResultsJson(ByteString.copyFromUtf8(Integer.toString(attempt)))
                            .addResultsJson(ByteString.copyFromUtf8(attempt == 1 ? "" : "3"))
                            .addErrors("")
                            .addErrors(attempt == 1 ? "missing document" : "")
                            .build());
        }
    }
}
