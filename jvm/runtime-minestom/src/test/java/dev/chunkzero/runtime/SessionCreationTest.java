package dev.chunkzero.runtime;

import static org.junit.jupiter.api.Assertions.*;

import chunk.v1.Common.SessionRef;
import chunk.v1.Supervision.SessionCommand;

import com.google.protobuf.ByteString;

import dev.chunkzero.backend.api.JsonType;
import dev.chunkzero.runtime.bootstrap.FlatSession;

import net.minestom.server.MinecraftServer;

import org.junit.jupiter.api.Test;

import tools.jackson.core.type.TypeReference;

import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.Objects;

class SessionCreationTest {
    public record Config(String map, String mode) {
        public Config {
            Objects.requireNonNull(map);
            Objects.requireNonNull(mode);
        }
    }

    @Test
    void oneProviderReceivesIndependentSettingsAndRejectsChangesBeforeConstruction() {
        MinecraftServer.init();
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
            MinecraftServer.process().stop();
        }
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
