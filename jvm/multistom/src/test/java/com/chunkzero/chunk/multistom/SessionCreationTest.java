package com.chunkzero.chunk.multistom;

import static org.junit.jupiter.api.Assertions.*;

import chunk.sync.v1.Jvm.JvmSession;
import chunk.sync.v1.Jvm.JvmSessionPhase;
import chunk.sync.v1.Jvm.JvmSessionStatus;

import com.chunkzero.chunk.backend.api.JsonType;
import com.chunkzero.chunk.multistom.bootstrap.FlatSession;
import com.chunkzero.chunk.multistom.event.SessionCreateEvent;
import com.chunkzero.chunk.runtime.ChunkSessions;
import com.chunkzero.chunk.runtime.ConfiguredSessionProvider;
import com.chunkzero.chunk.runtime.SessionCreation;
import com.chunkzero.chunk.runtime.SessionMethodRegistry;
import com.chunkzero.chunk.runtime.SessionProvider;
import com.chunkzero.chunk.runtime.SessionRegistry;
import com.chunkzero.chunk.runtime.TestHosts;
import com.google.protobuf.ByteString;

import net.minestom.server.ServerProcess;
import net.minestom.server.instance.InstanceContainer;

import org.junit.jupiter.api.Test;

import tools.jackson.core.type.TypeReference;

import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionException;
import java.util.concurrent.CompletionStage;
import java.util.concurrent.TimeoutException;
import java.util.concurrent.atomic.AtomicLong;
import java.util.function.Supplier;
import java.util.stream.Collectors;

class SessionCreationTest {
    public record Config(String map, String mode) {
        public Config {
            Objects.requireNonNull(map);
            Objects.requireNonNull(mode);
        }
    }

    @Test
    void oneProviderReceivesIndependentSettingsAndRejectsChangesBeforeConstruction() {
        var process = ServerProcess.create();
        var ticks = new TickExecutor();
        var settings = new ArrayList<SessionCreation<Config>>();
        var provider =
                new ConfiguredSessionProvider<Config, Session>() {
                    @Override
                    public JsonType<Config> configurationType() {
                        return JsonType.of(new TypeReference<Config>() {}, Objects::requireNonNull);
                    }

                    @Override
                    public Session create(SessionCreation<Config> creation) {
                        settings.add(creation);
                        return new FlatSession();
                    }
                };
        var manager =
                new SessionManager(
                        process,
                        ticks,
                        new SessionRegistry(
                                Map.of(
                                        "arena/default",
                                        provider,
                                        "arena/legacy",
                                        (SessionProvider<Session>) FlatSession::new)),
                        new SessionMethodRegistry(List.of(), List.of()));
        var host = TestHosts.detached(manager);
        try {
            var first = session(16, "{\"map\":\"forest\",\"mode\":\"solo\"}");
            var second = session(32, "{\"map\":\"desert\",\"mode\":\"solo\"}");
            var firstResult = TestHosts.create(host, "first", first);
            var secondResult = TestHosts.create(host, "second", second);
            flush(ticks);
            assertEquals(16, firstResult.join().getCapacity());
            assertEquals(32, secondResult.join().getCapacity());
            assertEquals(
                    List.of(
                            new SessionCreation<>(16, new Config("forest", "solo")),
                            new SessionCreation<>(32, new Config("desert", "solo"))),
                    settings);
            var replay =
                    TestHosts.create(
                            host,
                            "first",
                            first.toBuilder()
                                    .setConfigurationJson(
                                            ByteString.copyFromUtf8(
                                                    "{\"mode\":\"solo\",\"map\":\"forest\"}"))
                                    .build());
            flush(ticks);
            assertEquals(firstResult.join(), replay.join());
            var changed =
                    TestHosts.create(
                            host,
                            "first",
                            first.toBuilder()
                                    .setConfigurationJson(second.getConfigurationJson())
                                    .build());
            flush(ticks);
            assertTrue(changed.isCompletedExceptionally());
            var invalidValues =
                    List.of(
                            "{}",
                            "null",
                            "[]",
                            "{\"map\":1,\"mode\":\"solo\"}",
                            "{\"map\":\"forest\",\"mode\":\"solo\",\"extra\":true}",
                            "{\"map\":\"" + "x".repeat(65_536) + "\",\"mode\":\"solo\"}");
            for (var json : invalidValues) {
                var invalid = TestHosts.create(host, "invalid", session(16, json));
                flush(ticks);
                assertTrue(invalid.isCompletedExceptionally());
            }
            var invalidEncoding =
                    TestHosts.create(
                            host,
                            "invalid",
                            session(16, "{}").toBuilder()
                                    .setConfigurationJson(
                                            ByteString.copyFrom(new byte[] {(byte) 0xFF}))
                                    .build());
            flush(ticks);
            assertTrue(invalidEncoding.isCompletedExceptionally());
            assertEquals(2, settings.size());
            var legacy =
                    TestHosts.create(
                            host,
                            "legacy",
                            session(16, "{}").toBuilder().setSessionType("arena/legacy").build());
            var invalidLegacy =
                    TestHosts.create(
                            host,
                            "invalidLegacy",
                            session(16, "{\"map\":\"forest\"}").toBuilder()
                                    .setSessionType("arena/legacy")
                                    .build());
            flush(ticks);
            legacy.join();
            assertTrue(invalidLegacy.isCompletedExceptionally());
            TestHosts.finish(host, "first", first);
            TestHosts.finish(host, "second", second);
            TestHosts.finish(host, "legacy", session(16, "{}"));
            flush(ticks);
        } finally {
            process.stop();
        }
    }

    @Test
    void failedCreationAndUnknownFinishAreReported() {
        var process = ServerProcess.create();
        var ticks = new TickExecutor();
        var manager =
                new SessionManager(
                        process,
                        ticks,
                        new SessionRegistry(
                                Map.of(
                                        "arena/default",
                                        (SessionProvider<Session>)
                                                () -> {
                                                    throw new IllegalStateException(
                                                            "provider failed");
                                                },
                                        "arena/foreign",
                                        (SessionProvider<String>) () -> "not a session")),
                        new SessionMethodRegistry(List.of(), List.of()));
        var host = TestHosts.detached(manager);
        try {
            var failed = TestHosts.create(host, "failed", session(16, "{}"));
            var foreign =
                    TestHosts.create(
                            host,
                            "foreign",
                            session(16, "{}").toBuilder().setSessionType("arena/foreign").build());
            var unknown = TestHosts.finish(host, "unknown", session(16, "{}"));
            flush(ticks);
            assertTrue(failed.isCompletedExceptionally());
            var error = assertThrows(CompletionException.class, foreign::join);
            assertEquals(
                    "Session type arena/foreign must create a " + Session.class.getName(),
                    error.getCause().getMessage());
            assertEquals(JvmSessionPhase.JVM_SESSION_PHASE_ENDED, unknown.join().getPhase());
            assertEquals(
                    Map.of(
                            "failed", JvmSessionPhase.JVM_SESSION_PHASE_FAILED,
                            "foreign", JvmSessionPhase.JVM_SESSION_PHASE_FAILED,
                            "unknown", JvmSessionPhase.JVM_SESSION_PHASE_ENDED),
                    phases(host));
        } finally {
            process.stop();
        }
    }

    @Test
    void noSessionIsReportedTerminalBeforeItsCleanupCompletes() {
        var process = ServerProcess.create();
        var ticks = new TickExecutor();
        var cleanup = new CompletableFuture<Void>();
        var instances = new ArrayList<InstanceContainer>();
        Supplier<Session> failing =
                () ->
                        new Session() {
                            @Override
                            public CompletionStage<Void> onCreate(SessionScope scope) {
                                instances.add(scope.createInstance());
                                return CompletableFuture.failedFuture(
                                        new IllegalStateException("create failed"));
                            }

                            @Override
                            public CompletionStage<Void> onFinish() {
                                return cleanup;
                            }
                        };
        Supplier<Session> leaking =
                () ->
                        new Session() {
                            @Override
                            public CompletionStage<Void> onCreate(SessionScope scope) {
                                scope.createInstance();
                                AutoCloseable leak =
                                        () -> {
                                            throw new IllegalStateException("close failed");
                                        };
                                scope.own(leak);
                                return CompletableFuture.completedFuture(null);
                            }
                        };
        var manager =
                new SessionManager(
                        process, ticks, Map.of("arena/default", failing, "arena/leaky", leaking));
        var host = TestHosts.detached(manager);
        try {
            var leaky = session(16, "{}").toBuilder().setSessionType("arena/leaky").build();
            TestHosts.create(host, "failed", session(16, "{}"));
            TestHosts.create(host, "leaky", leaky);
            TestHosts.finish(host, "leaky", leaky);
            for (var tick = 0; tick < 20; tick++) {
                ticks.flush();
                if (tick == 10) cleanup.complete(null);
                var phases = phases(host);
                if (TestHosts.terminal(phases.get("failed")))
                    assertFalse(instances.getFirst().isRegistered());
                assertFalse(TestHosts.terminal(phases.get("leaky")));
            }
            assertEquals(JvmSessionPhase.JVM_SESSION_PHASE_FAILED, phases(host).get("failed"));
            assertEquals(JvmSessionPhase.JVM_SESSION_PHASE_ENDING, phases(host).get("leaky"));
        } finally {
            process.stop();
        }
    }

    @Test
    void sessionWhoseCreationOutlastsTheDeadlineFailsAndIgnoresLaterCompletion() {
        var process = ServerProcess.create();
        var ticks = new TickExecutor();
        var creation = new CompletableFuture<Void>();
        var instances = new ArrayList<InstanceContainer>();
        var manager =
                new SessionManager(
                        process,
                        ticks,
                        Map.<String, Supplier<Session>>of(
                                "arena/default",
                                () ->
                                        new Session() {
                                            @Override
                                            public CompletionStage<Void> onCreate(
                                                    SessionScope scope) {
                                                instances.add(scope.createInstance());
                                                return creation;
                                            }
                                        }));
        var clock = new AtomicLong();
        var host = TestHosts.withDeadline(manager, Duration.ofMillis(50), clock::get);
        try {
            var created = TestHosts.create(host, "slow", session(16, "{}"));
            ticks.flush();
            TestHosts.sweep(host);
            assertFalse(created.isDone());
            clock.addAndGet(Duration.ofMillis(50).toNanos());
            TestHosts.sweep(host);
            assertTrue(created.isDone());
            var failure = assertThrows(CompletionException.class, created::join);
            assertInstanceOf(TimeoutException.class, failure.getCause());
            flush(ticks);
            assertEquals(JvmSessionPhase.JVM_SESSION_PHASE_FAILED, TestHosts.phase(host, "slow"));
            assertFalse(instances.getFirst().isRegistered());
            assertFalse(creation.isDone());
            assertTrue(creation.complete(null));
            flush(ticks);
            assertEquals(JvmSessionPhase.JVM_SESSION_PHASE_FAILED, TestHosts.phase(host, "slow"));
        } finally {
            process.stop();
        }
    }

    @Test
    void creationCompletingAfterTheDeadlinePublishesNoCreateEvent() {
        var process = ServerProcess.create();
        var ticks = new TickExecutor();
        var manager =
                new SessionManager(
                        process,
                        ticks,
                        Map.<String, Supplier<Session>>of("arena/default", FlatSession::new));
        var clock = new AtomicLong();
        var host = TestHosts.withDeadline(manager, Duration.ofMillis(50), clock::get);
        var events = new ArrayList<SessionScope>();
        process.eventHandler()
                .addListener(SessionCreateEvent.class, event -> events.add(event.getSession()));
        try {
            var created = TestHosts.create(host, "late", session(16, "{}"));
            // The creation has completed, but its tick task has not run yet.
            ticks.flush();
            clock.addAndGet(Duration.ofMillis(50).toNanos());
            TestHosts.sweep(host);
            flush(ticks);
            assertThrows(CompletionException.class, created::join);
            assertEquals(JvmSessionPhase.JVM_SESSION_PHASE_FAILED, TestHosts.phase(host, "late"));
            assertTrue(events.isEmpty());
        } finally {
            process.stop();
        }
    }

    @Test
    void forgottenSessionsLeaveRoomForNewOnes() {
        var process = ServerProcess.create();
        var ticks = new TickExecutor();
        var manager =
                new SessionManager(
                        process,
                        ticks,
                        Map.<String, Supplier<Session>>of("arena/default", FlatSession::new));
        var host = TestHosts.detached(manager);
        try {
            for (var index = 0; index < 300; index++) {
                var id = "session" + index;
                var session = session(16, "{}");
                var created = TestHosts.create(host, id, session);
                flush(ticks);
                created.join();
                TestHosts.finish(host, id, session);
                flush(ticks);
                TestHosts.forget(host);
                flush(ticks);
            }
            assertTrue(TestHosts.inventory(host).getSessionsList().isEmpty());
        } finally {
            process.stop();
        }
    }

    private static Map<String, JvmSessionPhase> phases(ChunkSessions host) {
        return TestHosts.inventory(host).getSessionsList().stream()
                .collect(Collectors.toMap(JvmSessionStatus::getId, JvmSessionStatus::getPhase));
    }

    private static JvmSession session(int capacity, String config) {
        return JvmSession.newBuilder()
                .setSessionType("arena/default")
                .setCapacity(capacity)
                .setConfigurationJson(ByteString.copyFromUtf8(config))
                .build();
    }

    private static void flush(TickExecutor ticks) {
        for (var index = 0; index < 10; index++) ticks.flush();
    }
}
