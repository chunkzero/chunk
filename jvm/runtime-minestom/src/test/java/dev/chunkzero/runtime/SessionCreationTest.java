package dev.chunkzero.runtime;

import static org.junit.jupiter.api.Assertions.*;

import chunk.v1.Common.SessionRef;
import chunk.v1.Supervision.SessionCommand;
import chunk.v1.Supervision.SessionInventory;
import chunk.v1.Supervision.SessionPhase;

import com.google.protobuf.ByteString;

import dev.chunkzero.backend.api.JsonType;
import dev.chunkzero.runtime.bootstrap.FlatSession;

import net.minestom.server.ServerProcess;
import net.minestom.server.instance.InstanceContainer;

import org.junit.jupiter.api.Test;

import tools.jackson.core.type.TypeReference;

import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionStage;
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
                new ConfiguredSessionProvider<Config>() {
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
                        Map.of(
                                "arena/default",
                                SessionRegistration.provider("arena", () -> provider),
                                "arena/legacy",
                                new SessionRegistration("arena", FlatSession::new)),
                        null);
        try {
            var first = command("first", 16, "{\"map\":\"forest\",\"mode\":\"solo\"}");
            var second = command("second", 32, "{\"map\":\"desert\",\"mode\":\"solo\"}");
            var firstResult = manager.create(first);
            var secondResult = manager.create(second);
            flush(ticks);
            assertEquals(16, firstResult.join().getCapacity());
            assertEquals(32, secondResult.join().getCapacity());
            assertEquals(
                    List.of(
                            new SessionCreation<>(16, new Config("forest", "solo")),
                            new SessionCreation<>(32, new Config("desert", "solo"))),
                    settings);
            var replay =
                    manager.create(
                            first.toBuilder()
                                    .setConfigurationJson(
                                            ByteString.copyFromUtf8(
                                                    "{\"mode\":\"solo\",\"map\":\"forest\"}"))
                                    .build());
            flush(ticks);
            assertEquals(firstResult.join(), replay.join());
            var changed =
                    manager.create(
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
                var invalid = manager.create(command("invalid", 16, json));
                flush(ticks);
                assertTrue(invalid.isCompletedExceptionally());
            }
            var invalidEncoding =
                    manager.create(
                            command("invalid", 16, "{}").toBuilder()
                                    .setConfigurationJson(
                                            ByteString.copyFrom(new byte[] {(byte) 0xFF}))
                                    .build());
            flush(ticks);
            assertTrue(invalidEncoding.isCompletedExceptionally());
            assertEquals(2, settings.size());
            var legacy =
                    manager.create(
                            command("legacy", 16, "{}").toBuilder()
                                    .setSessionType("arena/legacy")
                                    .build());
            var invalidLegacy =
                    manager.create(
                            command("invalidLegacy", 16, "{\"map\":\"forest\"}").toBuilder()
                                    .setSessionType("arena/legacy")
                                    .build());
            flush(ticks);
            legacy.join();
            assertTrue(invalidLegacy.isCompletedExceptionally());
            manager.finish(first);
            manager.finish(second);
            manager.finish(command("legacy", 16, "{}"));
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
                        Map.of(
                                "arena/default",
                                new SessionRegistration(
                                        "arena",
                                        () -> {
                                            throw new IllegalStateException("provider failed");
                                        })),
                        null);
        try {
            var failed = manager.create(command("failed", 16, "{}"));
            var unknown = manager.finish(command("unknown", 16, "{}"));
            flush(ticks);
            assertTrue(failed.isCompletedExceptionally());
            assertEquals(SessionPhase.SESSION_PHASE_ENDED, unknown.join().getPhase());
            assertEquals(
                    Map.of(
                            "failed", SessionPhase.SESSION_PHASE_FAILED,
                            "unknown", SessionPhase.SESSION_PHASE_ENDED),
                    phases(manager));
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
                        process,
                        ticks,
                        Map.of(
                                "arena/default",
                                new SessionRegistration("arena", failing),
                                "arena/leaky",
                                new SessionRegistration("arena", leaking)),
                        null);
        try {
            var leaky =
                    command("leaky", 16, "{}").toBuilder().setSessionType("arena/leaky").build();
            manager.create(command("failed", 16, "{}"));
            manager.create(leaky);
            manager.finish(leaky);
            for (var tick = 0; tick < 20; tick++) {
                ticks.flush();
                if (tick == 10) cleanup.complete(null);
                var phases = phases(manager);
                if (terminal(phases.get("failed")))
                    assertFalse(instances.getFirst().isRegistered());
                assertFalse(terminal(phases.get("leaky")));
            }
            assertEquals(SessionPhase.SESSION_PHASE_FAILED, phases(manager).get("failed"));
            assertEquals(SessionPhase.SESSION_PHASE_ENDING, phases(manager).get("leaky"));
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
                        Map.of("arena/default", new SessionRegistration("arena", FlatSession::new)),
                        null);
        try {
            for (var index = 0; index < 300; index++) {
                var command = command("session" + index, 16, "{}");
                var created = manager.create(command);
                flush(ticks);
                created.join();
                manager.finish(command);
                flush(ticks);
                manager.forget(command.getSession().getId());
                flush(ticks);
            }
            assertTrue(manager.inventory().isEmpty());
        } finally {
            process.stop();
        }
    }

    private static Map<String, SessionPhase> phases(SessionManager manager) {
        return manager.inventory().stream()
                .collect(
                        Collectors.toMap(
                                session -> session.getSession().getId(),
                                SessionInventory::getPhase));
    }

    private static boolean terminal(SessionPhase phase) {
        return phase == SessionPhase.SESSION_PHASE_ENDED
                || phase == SessionPhase.SESSION_PHASE_FAILED;
    }

    private static SessionCommand command(String id, int capacity, String config) {
        return SessionCommand.newBuilder()
                .setSession(SessionRef.newBuilder().setId(id))
                .setOperationId(id)
                .setGeneration(1)
                .setSessionType("arena/default")
                .setCapacity(capacity)
                .setConfigurationJson(ByteString.copyFromUtf8(config))
                .build();
    }

    private static void flush(TickExecutor ticks) {
        for (var index = 0; index < 10; index++) ticks.flush();
    }
}
