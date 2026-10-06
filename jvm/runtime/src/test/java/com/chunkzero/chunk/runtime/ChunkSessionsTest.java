package com.chunkzero.chunk.runtime;

import static org.junit.jupiter.api.Assertions.*;

import chunk.sync.v1.CoreOuterClass.Position;
import chunk.sync.v1.Gateway.PlayerIdentity;
import chunk.sync.v1.Jvm.JvmDelivery;
import chunk.sync.v1.Jvm.JvmDeliveryPhase;
import chunk.sync.v1.Jvm.JvmDeliveryStatus;
import chunk.sync.v1.Jvm.JvmMethodCall;
import chunk.sync.v1.Jvm.JvmMethodPhase;
import chunk.sync.v1.Jvm.JvmMethodResult;
import chunk.sync.v1.Jvm.JvmSession;
import chunk.sync.v1.Jvm.JvmSessionPhase;
import chunk.sync.v1.Jvm.PlayerSetup;

import com.chunkzero.chunk.backend.api.Destination;
import com.chunkzero.chunk.backend.client.BackendSession;
import com.google.protobuf.ByteString;
import com.google.protobuf.Message;

import org.junit.jupiter.api.Test;

import java.time.Duration;
import java.util.ArrayDeque;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import java.util.UUID;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionStage;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.Executor;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;

class ChunkSessionsTest {
    private final UUID uuid = UUID.randomUUID();
    private final AtomicLong clock = new AtomicLong();
    private final Map<String, JvmMethodResult> results = new ConcurrentHashMap<>();
    private final Map<String, ByteString> topic = new TreeMap<>();

    @Test
    void sessionsAdmitDeliveredPlayersAndEndOnlyOnceTheyAreReleased() {
        var handler = new Handler();
        var host = host(handler, Duration.ofSeconds(10));
        put("session/a", session(1, false));
        host.apply(topic);
        var control = handler.created.getFirst();
        assertEquals("app/default", control.type());
        assertEquals(1, control.capacity());
        assertEquals("{}", control.configurationJson());
        assertEquals(JvmSessionPhase.JVM_SESSION_PHASE_STARTING, host.phase("a"));
        // A delivery waits for its session to become ready.
        put("delivery/first", delivery("a", 1));
        host.apply(topic);
        assertTrue(host.inventory().getDeliveriesList().isEmpty());
        control.ready();
        host.sweep();
        assertEquals(JvmSessionPhase.JVM_SESSION_PHASE_READY, host.phase("a"));
        var prepared = status(host, "first");
        assertEquals(JvmDeliveryPhase.JVM_DELIVERY_PHASE_PREPARED, prepared.getPhase());
        // The session has room for one player.
        put("delivery/second", delivery("a", 2));
        host.apply(topic);
        assertEquals(JvmDeliveryPhase.JVM_DELIVERY_PHASE_CLOSED, status(host, "second").getPhase());

        var setup = setup("first", prepared.getCapability());
        var disconnects = new AtomicInteger();
        assertThrows(
                IllegalArgumentException.class,
                () -> host.admit(setup("first", ByteString.EMPTY), uuid, "player", () -> {}));
        assertThrows(
                IllegalArgumentException.class, () -> host.admit(setup, uuid, "other", () -> {}));
        var delivery = host.admit(setup, uuid, "player", disconnects::incrementAndGet);
        assertEquals("a", delivery.session());
        assertEquals("player", delivery.player().name());
        assertEquals("a/" + uuid + "/1.1/coin", delivery.operationId("coin").value());
        assertThrows(
                IllegalStateException.class, () -> host.admit(setup, uuid, "player", () -> {}));
        assertEquals(
                JvmDeliveryPhase.JVM_DELIVERY_PHASE_ATTACHED, status(host, "first").getPhase());
        assertEquals(1, host.players());

        // Methods run only for an arrived player.
        put("method/early", call("first"));
        host.apply(topic);
        assertEquals(JvmMethodPhase.JVM_METHOD_PHASE_CANCELLED, results.get("early").getPhase());
        delivery.arrived();
        put("method/late", call("first"));
        host.apply(topic);
        assertEquals("\"ok\"", results.get("late").getResultJson().toStringUtf8());

        // Ending withdraws the player, and the handler finishes only after the release.
        put("session/a", session(1, true));
        host.apply(topic);
        assertEquals(1, disconnects.get());
        assertEquals(JvmSessionPhase.JVM_SESSION_PHASE_ENDING, host.phase("a"));
        assertEquals(
                JvmDeliveryPhase.JVM_DELIVERY_PHASE_WITHDRAWING, status(host, "first").getPhase());
        assertTrue(handler.finished.isEmpty());
        delivery.release();
        assertEquals(List.of(control), handler.finished);
        assertEquals(JvmSessionPhase.JVM_SESSION_PHASE_ENDING, host.phase("a"));
        control.ended();
        assertEquals(JvmSessionPhase.JVM_SESSION_PHASE_ENDED, host.phase("a"));
        assertEquals(JvmDeliveryPhase.JVM_DELIVERY_PHASE_CLOSED, status(host, "first").getPhase());
        assertEquals(0, host.activeCount());

        topic.clear();
        host.apply(topic);
        host.state().inventory();
        assertTrue(host.inventory().getSessionsList().isEmpty());
        assertTrue(host.inventory().getDeliveriesList().isEmpty());
    }

    @Test
    void unconnectedDeliveriesExpireAndLateCreationsFail() throws Exception {
        var handler = new Handler();
        var host = host(handler, Duration.ofMillis(50));
        put("session/slow", session(4, false));
        put("session/fast", session(4, false));
        host.apply(topic);
        handler.created.stream()
                .filter(session -> session.id().equals("fast"))
                .forEach(SessionControl::ready);
        put("delivery/expired", delivery("fast", 1));
        host.apply(topic);
        var capability = status(host, "expired").getCapability();
        clock.addAndGet(TimeUnit.SECONDS.toNanos(61));
        host.sweep();
        assertEquals(
                JvmDeliveryPhase.JVM_DELIVERY_PHASE_CLOSED, status(host, "expired").getPhase());
        assertThrows(
                IllegalStateException.class,
                () -> host.admit(setup("expired", capability), uuid, "player", () -> {}));

        var deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        while (handler.finished.isEmpty() && System.nanoTime() < deadline) Thread.sleep(5);
        var slow = handler.finished.getFirst();
        assertEquals("slow", slow.id());
        slow.ended();
        assertEquals(JvmSessionPhase.JVM_SESSION_PHASE_FAILED, host.phase("slow"));
        slow.ready();
        assertEquals(JvmSessionPhase.JVM_SESSION_PHASE_FAILED, host.phase("slow"));
        assertEquals(JvmSessionPhase.JVM_SESSION_PHASE_READY, host.phase("fast"));
    }

    @Test
    void activeDeliveriesFenceTheirPlayerAndOnlyNewerOnesReplaceThem() {
        var fence = new DeliveryFence();
        fence.claim("player", at(1, 1));
        assertThrows(IllegalArgumentException.class, () -> fence.claim("player", at(1, 1)));
        assertThrows(IllegalArgumentException.class, () -> fence.claim("player", at(1, 2)));
        fence.release("player", at(1, 1));
        assertThrows(IllegalArgumentException.class, () -> fence.claim("player", at(1, 1)));
        fence.claim("player", at(1, 2));
        fence.release("player", at(1, 1));
        assertThrows(IllegalArgumentException.class, () -> fence.claim("player", at(1, 3)));
        fence.release("player", at(1, 2));
        // A restore's new epoch orders after every revision of the previous one.
        fence.claim("player", at(2, 1));
        assertThrows(
                IllegalArgumentException.class,
                () -> fence.claim("unset", Position.getDefaultInstance()));
    }

    @Test
    void methodsQueuedBeforeAPlayerLeavesDoNotStartAndTheDeliveryStaysFenced() {
        var handler = new Handler();
        var host = host(handler, Duration.ofSeconds(10));
        put("session/a", session(1, false));
        host.apply(topic);
        handler.created.getFirst().ready();
        put("delivery/first", delivery("a", 1));
        host.apply(topic);
        var delivery =
                host.admit(
                        setup("first", status(host, "first").getCapability()),
                        uuid,
                        "player",
                        () -> {});
        delivery.arrived();

        handler.gate = new CompletableFuture<>();
        put("method/queued", call("first"));
        host.apply(topic);
        delivery.left();
        delivery.left();
        handler.gate.complete(null);
        assertEquals(JvmMethodPhase.JVM_METHOD_PHASE_CANCELLED, results.get("queued").getPhase());
        assertTrue(handler.effects.isEmpty());
        assertEquals(JvmDeliveryPhase.JVM_DELIVERY_PHASE_ARRIVED, status(host, "first").getPhase());
        delivery.release();
        assertEquals(JvmDeliveryPhase.JVM_DELIVERY_PHASE_CLOSED, status(host, "first").getPhase());
    }

    @Test
    void methodsCancelledBeforeTheyStartDoNotRun() {
        var handler = new Handler();
        var host = host(handler, Duration.ofSeconds(10));
        put("session/a", session(1, false));
        host.apply(topic);
        handler.created.getFirst().ready();
        put("delivery/first", delivery("a", 1));
        host.apply(topic);
        host.admit(setup("first", status(host, "first").getCapability()), uuid, "player", () -> {})
                .arrived();

        handler.gate = new CompletableFuture<>();
        put("method/slow", call("first"));
        host.apply(topic);
        put("method/slow", call("first").toBuilder().setCancel(true).build());
        host.apply(topic);
        handler.gate.complete(null);
        assertEquals(JvmMethodPhase.JVM_METHOD_PHASE_CANCELLED, results.get("slow").getPhase());
        assertTrue(handler.effects.isEmpty());

        handler.gate = new CompletableFuture<>();
        put("method/late", call("first"));
        host.apply(topic);
        clock.addAndGet(TimeUnit.SECONDS.toNanos(61));
        put("delivery/first", delivery("a", 1).toBuilder().setWithdraw(true).build());
        host.apply(topic);
        handler.gate.complete(null);
        assertEquals(JvmMethodPhase.JVM_METHOD_PHASE_CANCELLED, results.get("late").getPhase());
        assertTrue(handler.effects.isEmpty());
    }

    @Test
    void creationDispatchedAfterTeardownDoesNotRun() {
        var handler = new Handler();
        var queue = new ArrayDeque<Runnable>();
        var host = host(handler, Duration.ofMillis(50), queue::add);
        put("session/a", session(1, false));
        host.apply(topic);
        clock.addAndGet(TimeUnit.SECONDS.toNanos(1));
        host.sweep();
        while (!queue.isEmpty()) queue.poll().run();
        assertTrue(handler.created.isEmpty());
        assertEquals(1, handler.finished.size());
    }

    @Test
    void methodsTheirHandlerNeverStartedAreCancelledOrFailed() {
        var handler =
                new Handler() {
                    @Override
                    public CompletionStage<String> method(
                            SessionControl session, SessionMethod call) {
                        return call.name().equals("skip")
                                ? CompletableFuture.completedFuture(null)
                                : CompletableFuture.failedFuture(
                                        new IllegalStateException("undeclared"));
                    }
                };
        var host = host(handler, Duration.ofSeconds(10));
        put("session/a", session(1, false));
        host.apply(topic);
        handler.created.getFirst().ready();
        put("delivery/first", delivery("a", 1));
        host.apply(topic);
        host.admit(setup("first", status(host, "first").getCapability()), uuid, "player", () -> {})
                .arrived();

        put("method/skip", call("first").toBuilder().setMethod("skip").build());
        put("method/undeclared", call("first").toBuilder().setMethod("undeclared").build());
        host.apply(topic);
        assertEquals(JvmMethodPhase.JVM_METHOD_PHASE_CANCELLED, results.get("skip").getPhase());
        assertEquals(JvmMethodPhase.JVM_METHOD_PHASE_FAILED, results.get("undeclared").getPhase());
    }

    private ChunkSessions host(SessionHandler handler, Duration deadline) {
        return host(handler, deadline, Runnable::run);
    }

    private ChunkSessions host(SessionHandler handler, Duration deadline, Executor callbacks) {
        return new ChunkSessions(
                handler,
                new ChunkSessions.Link() {
                    @Override
                    public boolean acceptsWork() {
                        return true;
                    }

                    @Override
                    public BackendSession backend(String session) {
                        return null;
                    }

                    @Override
                    public CompletionStage<MoveResult> move(
                            String delivery, Position generation, Destination destination) {
                        return CompletableFuture.completedFuture(MoveResult.ACCEPTED);
                    }

                    @Override
                    public void methodResult(String operation, JvmMethodResult result) {
                        results.put(operation, result);
                    }

                    @Override
                    public void flush() {}

                    @Override
                    public long nanoTime() {
                        return clock.get();
                    }
                },
                callbacks,
                deadline,
                null);
    }

    private void put(String key, Message value) {
        topic.put(key, value.toByteString());
    }

    private static JvmSession session(int capacity, boolean finish) {
        return JvmSession.newBuilder()
                .setSessionType("app/default")
                .setCapacity(capacity)
                .setFinish(finish)
                .build();
    }

    private JvmDelivery delivery(String session, long revision) {
        return JvmDelivery.newBuilder()
                .setSession(session)
                .setGeneration(at(1, revision))
                .setPlayer(
                        PlayerIdentity.newBuilder().setUuid(uuid.toString()).setUsername("player"))
                .build();
    }

    private static JvmMethodCall call(String delivery) {
        return JvmMethodCall.newBuilder()
                .setSession("a")
                .setMethod("greet")
                .setArgumentsJson(ByteString.copyFromUtf8("{}"))
                .setDelivery(delivery)
                .setDeadlineMs(System.currentTimeMillis() + 30_000)
                .build();
    }

    private static byte[] setup(String operation, ByteString capability) {
        return PlayerSetup.newBuilder()
                .setOperationId(operation)
                .setCapability(capability)
                .build()
                .toByteArray();
    }

    private static JvmDeliveryStatus status(ChunkSessions host, String operation) {
        return host.inventory().getDeliveriesList().stream()
                .filter(status -> status.getOperationId().equals(operation))
                .findFirst()
                .orElseThrow();
    }

    private static Position at(long epoch, long revision) {
        return Position.newBuilder().setEpoch(epoch).setRevision(revision).build();
    }

    private static class Handler implements SessionHandler {
        final List<SessionControl> created = new CopyOnWriteArrayList<>();
        final List<SessionControl> finished = new CopyOnWriteArrayList<>();
        final List<String> effects = new CopyOnWriteArrayList<>();
        volatile CompletableFuture<Void> gate;

        @Override
        public void create(SessionControl session) {
            created.add(session);
        }

        @Override
        public void finish(SessionControl session) {
            finished.add(session);
        }

        @Override
        public CompletionStage<String> method(SessionControl session, SessionMethod call) {
            var run = gate == null ? CompletableFuture.<Void>completedFuture(null) : gate;
            return run.thenApply(
                    ignored -> {
                        if (!call.start()) return null;
                        effects.add(call.name());
                        return "\"ok\"";
                    });
        }
    }
}
