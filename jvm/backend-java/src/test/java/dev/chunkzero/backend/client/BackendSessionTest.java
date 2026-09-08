package dev.chunkzero.backend.client;

import chunk.v1.BackendGrpc;
import chunk.v1.BackendOuterClass.*;
import com.google.protobuf.ByteString;
import dev.chunkzero.backend.api.*;
import dev.chunkzero.generated.BackendClient;
import dev.chunkzero.generated.BackendTypes;
import io.grpc.Context;
import io.grpc.ManagedChannel;
import io.grpc.ManagedChannelBuilder;
import io.grpc.Server;
import io.grpc.ServerBuilder;
import io.grpc.Status;
import io.grpc.stub.ServerCallStreamObserver;
import io.grpc.stub.StreamObserver;
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
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import static org.junit.jupiter.api.Assertions.*;

class BackendSessionTest {
    private final Fixture fixture = new Fixture();
    private Server server;
    private ManagedChannel channel;
    private ScheduledExecutorService scheduler;
    private BackendSession session;
    @BeforeEach void start() throws Exception {
        server = ServerBuilder.forPort(0).addService(fixture).build().start();
        channel = ManagedChannelBuilder.forAddress("127.0.0.1", server.getPort()).usePlaintext().build();
        scheduler = Executors.newSingleThreadScheduledExecutor();
        session = create(Duration.ofSeconds(5));
    }
    private BackendSession create(Duration deadline) {
        return new BackendSession(channel, "test-credential-with-at-least-32-bytes", "local", "immutable-build",
            new SessionIdentity(new SessionId("s1"), "duels", Optional.of(new PlayerId("trusted"))), scheduler, deadline);
    }
    @AfterEach void stop() throws Exception {
        session.close(); channel.shutdownNow().awaitTermination(2, TimeUnit.SECONDS);
        server.shutdownNow().awaitTermination(2, TimeUnit.SECONDS);
        scheduler.shutdownNow(); assertTrue(scheduler.awaitTermination(2, TimeUnit.SECONDS));
    }
    @Test void generatedJavaCallRecoversLostReplyWithoutChangingIdentity() throws Exception {
        var client = new BackendClient(session);
        var arguments = new BackendTypes.Fn$shared$profile$record$Args(1L, new Id<>("profiles:p1"), List.of(),
            FieldValue.absent(), new PlayerId("spoof"), new BackendTypes.Fn$shared$profile$record$Args$state.V0("ready"));
        var operation = OperationId.create();
        assertThrows(ExecutionException.class, () -> client.call$shared$profile$record(arguments, operation).get(2, TimeUnit.SECONDS));
        var result = client.call$shared$profile$record(arguments, operation).get(2, TimeUnit.SECONDS);
        assertTrue(result.ok()); assertEquals(new SessionId("s1"), result.session());
        assertEquals(1, fixture.saved.size());
        assertEquals(2, fixture.calls.size());
        assertEquals(fixture.calls.get(0), fixture.calls.get(1));
        var request = fixture.calls.get(0);
        assertEquals("immutable-build", request.getDeployment());
        assertEquals("trusted", Codecs.parse(request.getCallerJson().toStringUtf8()).getAsJsonObject().get("player").getAsString());
        assertEquals("spoof", Codecs.parse(request.getArgumentsJson().toStringUtf8()).getAsJsonObject().get("player").getAsString());
        assertTrue(fixture.deadlineObserved);
        assertEquals(3L, client.call$shared$profile$read(new BackendTypes.Fn$shared$profile$read$Args(new PlayerId("spoof"))).get(2, TimeUnit.SECONDS));
    }
    @Test void groupedWatchSignalsStaleThenReplacesOneConsistentSnapshot() throws Exception {
        var reference = new QueryRef<Long, Long>("shared/read", Codecs.INTEGER, Codecs.INTEGER);
        var first = session.bind(reference, 1L); var second = session.bind(reference, 2L);
        var states = new LinkedBlockingQueue<GroupState>();
        try (var watch = session.watchGroup(List.of(first, second), states::add)) {
            assertNotNull(watch);
            var initial = states.poll(2, TimeUnit.SECONDS); assertNotNull(initial);
            assertTrue(initial.stale()); assertTrue(initial.snapshot().isEmpty());
            var fresh = states.poll(2, TimeUnit.SECONDS); assertNotNull(fresh);
            assertFalse(fresh.stale()); assertEquals(1, fresh.snapshot().orElseThrow().revision());
            assertEquals(1L, fresh.snapshot().orElseThrow().result(first).valueOrThrow());
            assertInstanceOf(QueryResult.Failure.class, fresh.snapshot().orElseThrow().result(second));
            fixture.watch.onError(Status.UNAVAILABLE.asRuntimeException());
            var stale = states.poll(2, TimeUnit.SECONDS); assertNotNull(stale);
            assertTrue(stale.stale()); assertEquals(fresh.snapshot(), stale.snapshot());
            var recovered = states.poll(3, TimeUnit.SECONDS); assertNotNull(recovered);
            assertFalse(recovered.stale()); assertEquals(2, recovered.snapshot().orElseThrow().revision());
            assertEquals(2L, recovered.snapshot().orElseThrow().result(first).valueOrThrow());
            assertEquals(3L, recovered.snapshot().orElseThrow().result(second).valueOrThrow());
        }
    }
    @Test void playerDepartureCancelsItsCallsAndDeadlinesReachTheServer() throws Exception {
        var a = session.forPlayer(new PlayerId("a")); var b = session.forPlayer(new PlayerId("b"));
        var hang = new QueryRef<NullValue, Long>("shared/hang", Codecs.NULL, Codecs.INTEGER);
        var waiting = a.query(hang, NullValue.INSTANCE);
        assertTrue(fixture.hanging.await(2, TimeUnit.SECONDS));
        a.close(); assertTrue(waiting.isCancelled()); assertTrue(fixture.cancelled.await(2, TimeUnit.SECONDS));
        var read = new QueryRef<NullValue, Long>("shared/read", Codecs.NULL, Codecs.INTEGER);
        assertEquals(3L, b.query(read, NullValue.INSTANCE).get(2, TimeUnit.SECONDS));
        try (var shortDeadline = create(Duration.ofMillis(50))) {
            var error = assertThrows(ExecutionException.class, () -> shortDeadline.query(hang, NullValue.INSTANCE).get(2, TimeUnit.SECONDS));
            assertEquals(Status.Code.DEADLINE_EXCEEDED, Status.fromThrowable(error.getCause()).getCode());
        }
        session.close(); assertTrue(b.query(read, NullValue.INSTANCE).isCancelled());
    }
    private static final class Fixture extends BackendGrpc.BackendImplBase {
        final ConcurrentHashMap<String, BackendResult> saved = new ConcurrentHashMap<>();
        final CopyOnWriteArrayList<BackendCall> calls = new CopyOnWriteArrayList<>();
        final AtomicInteger watches = new AtomicInteger();
        final CountDownLatch hanging = new CountDownLatch(1);
        final CountDownLatch cancelled = new CountDownLatch(1);
        volatile StreamObserver<BackendUpdate> watch;
        volatile boolean deadlineObserved;
        @Override public void call(BackendCall request, StreamObserver<BackendResult> response) {
            deadlineObserved = Context.current().getDeadline() != null;
            if (request.getFunction().equals("shared/hang")) {
                ((ServerCallStreamObserver<BackendResult>) response).setOnCancelHandler(cancelled::countDown);
                hanging.countDown(); return;
            }
            if (request.getOperationId().isEmpty()) {
                response.onNext(BackendResult.newBuilder().setRevision(1).setResultJson(ByteString.copyFromUtf8("3")).build());
                response.onCompleted(); return;
            }
            calls.add(request);
            var result = BackendResult.newBuilder().setRevision(1).setResultJson(ByteString.copyFromUtf8("{\"ok\":true,\"session\":\"s1\"}")).build();
            if (saved.putIfAbsent(request.getOperationId(), result) == null) response.onError(Status.UNAVAILABLE.asRuntimeException());
            else { response.onNext(saved.get(request.getOperationId())); response.onCompleted(); }
        }
        @Override public void watch(BackendWatch request, StreamObserver<BackendUpdate> response) {
            int attempt = watches.incrementAndGet(); watch = response;
            response.onNext(BackendUpdate.newBuilder().setRevision(attempt)
                .addResultsJson(ByteString.copyFromUtf8(Integer.toString(attempt)))
                .addResultsJson(ByteString.copyFromUtf8(attempt == 1 ? "" : "3"))
                .addErrors("").addErrors(attempt == 1 ? "missing document" : "").build());
        }
    }
}
