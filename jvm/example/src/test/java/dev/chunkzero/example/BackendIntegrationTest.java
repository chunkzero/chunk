package dev.chunkzero.example;

import com.google.gson.JsonObject;
import com.google.gson.JsonParser;
import dev.chunkzero.backend.api.PlayerId;
import dev.chunkzero.backend.api.SessionId;
import dev.chunkzero.backend.client.BackendSession;
import dev.chunkzero.backend.client.OperationId;
import dev.chunkzero.backend.client.SessionIdentity;
import dev.chunkzero.backend.client.WatchState;
import dev.chunkzero.example.generated.BackendClient;
import dev.chunkzero.example.generated.BackendTypes.*;
import io.grpc.ManagedChannelBuilder;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.Optional;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.Executors;
import java.util.function.Predicate;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;
import static org.junit.jupiter.api.Assertions.*;

class BackendIntegrationTest {
    @TempDir Path directory;

    @Test void generatedJavaCallsTypeScriptAndRetainedWatchesRecoverAcrossBackendRestart() throws Exception {
        var scheduler = Executors.newSingleThreadScheduledExecutor();
        try (var backend = new BackendProcess(directory)) {
            var endpoint = backend.start("version-a", "127.0.0.1:0");
            var channel = ManagedChannelBuilder.forTarget(endpoint).usePlaintext().build();
            try (var session = new BackendSession(channel, backend.token, "test", "version-a",
                    new SessionIdentity(new SessionId("session-a"), "lobby", Optional.of(new PlayerId("trusted-player"))),
                    scheduler, Duration.ofSeconds(5))) {
                var client = new BackendClient(session);
                var updates = new LinkedBlockingQueue<WatchState<Fn$shared$players$stats$Result>>();
                try (var watch = client.watch$shared$players$stats(new Fn$shared$players$stats$Args(), updates::add)) {
                    await(updates, state -> !state.stale() && coins(state) == 0);
                    var operation = new OperationId("stable-reward");
                    assertEquals(1L, client.call$shared$players$coin(new Fn$shared$players$coin$Args(), operation).get());
                    await(updates, state -> !state.stale() && coins(state) == 1);
                    backend.stop();
                    await(updates, state -> state.stale() && coins(state) == 1);
                    backend.start("version-b", endpoint);
                    await(updates, state -> !state.stale() && coins(state) == 1);
                    try (var next = new BackendSession(channel, backend.token, "test", "version-b",
                            new SessionIdentity(new SessionId("session-b"), "arena", Optional.of(new PlayerId("trusted-player"))),
                            scheduler, Duration.ofSeconds(5))) {
                        var newer = new BackendClient(next);
                        assertEquals(2L, newer.call$shared$players$coin(new Fn$shared$players$coin$Args(), new OperationId("new-reward")).get());
                        await(updates, state -> !state.stale() && coins(state) == 2);
                        assertEquals(1L, client.call$shared$players$coin(new Fn$shared$players$coin$Args(), operation).get());
                        assertEquals(2L, newer.call$shared$players$stats(new Fn$shared$players$stats$Args()).get().coins());
                        try (var other = next.forPlayer(new PlayerId("other-player"))) {
                            assertEquals(0L, new BackendClient(other).call$shared$players$stats(new Fn$shared$players$stats$Args()).get().coins());
                        }
                    }
                }
            } finally {
                channel.shutdownNow().awaitTermination(3, TimeUnit.SECONDS);
            }
        } finally {
            scheduler.shutdownNow();
            scheduler.awaitTermination(3, TimeUnit.SECONDS);
        }
    }

    private static long coins(WatchState<Fn$shared$players$stats$Result> state) {
        return state.snapshot().map(snapshot -> snapshot.result().valueOrThrow().coins()).orElse(-1L);
    }

    private static <T> void await(LinkedBlockingQueue<T> values, Predicate<T> matches) throws Exception {
        var deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(10);
        while (System.nanoTime() < deadline) {
            var value = values.poll(100, TimeUnit.MILLISECONDS);
            if (value != null && matches.test(value)) return;
        }
        fail("Expected backend watch state did not arrive");
    }

    private static final class BackendProcess implements AutoCloseable {
        final Path directory;
        Process process;
        String token;

        BackendProcess(Path directory) { this.directory = directory; }

        String start(String id, String address) throws Exception {
            var generated = Path.of(System.getProperty("chunk.backend"));
            var bundle = JsonParser.parseString(Files.readString(generated.resolve("contract.json"))).getAsJsonObject();
            bundle.addProperty("id", id);
            bundle.addProperty("source", Files.readString(generated.resolve("source.mjs")));
            var bundlePath = directory.resolve("bundle.json");
            Files.writeString(bundlePath, bundle.toString());
            var connection = directory.resolve("connection.json");
            Files.deleteIfExists(connection);
            var builder = new ProcessBuilder(System.getProperty("chunk.executable"));
            builder.environment().putAll(java.util.Map.of(
                    "CHUNK_BUNDLE", bundlePath.toString(), "CHUNK_ENVIRONMENT", "test",
                    "CHUNK_STATE", directory.resolve("state").toString(),
                    "CHUNK_CONNECTION", connection.toString(), "CHUNK_BIND", address));
            process = builder.redirectErrorStream(true)
                    .redirectOutput(directory.resolve("backend.log").toFile()).start();
            var deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(10);
            while (!Files.exists(connection)) {
                assertTrue(process.isAlive(), "Backend process exited before readiness");
                assertTrue(System.nanoTime() < deadline, "Backend readiness timed out");
                Thread.sleep(10);
            }
            JsonObject record = JsonParser.parseString(Files.readString(connection)).getAsJsonObject();
            token = record.get("token").getAsString();
            return record.get("endpoint").getAsString().replace("http://", "");
        }

        void stop() throws Exception {
            if (process == null) return;
            process.destroy();
            if (!process.waitFor(5, TimeUnit.SECONDS)) {
                process.destroyForcibly();
                assertTrue(process.waitFor(3, TimeUnit.SECONDS), "Backend did not stop");
            }
            process = null;
        }

        public void close() throws Exception { stop(); }
    }
}
