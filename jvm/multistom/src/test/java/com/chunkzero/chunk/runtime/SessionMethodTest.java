package com.chunkzero.chunk.runtime;

import static org.junit.jupiter.api.Assertions.*;

import chunk.sync.v1.Jvm.JvmSession;

import com.chunkzero.chunk.backend.api.BackendValues;
import com.chunkzero.chunk.backend.api.JsonType;
import com.chunkzero.chunk.backend.api.SessionMethodRef;

import net.minestom.server.ServerProcess;

import org.junit.jupiter.api.Test;

import tools.jackson.core.type.TypeReference;

import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.concurrent.CompletableFuture;

class SessionMethodTest {
    @Test
    void declaredMethodsRunOnTheTickThreadWhileTheSessionIsReady() {
        var process = ServerProcess.create();
        var ticks = new TickExecutor();
        var game = new Game();
        var args = JsonType.of(new TypeReference<Args>() {}, Objects::requireNonNull);
        var number = JsonType.of(new TypeReference<Long>() {}, BackendValues::checkInteger);
        var binding =
                new SessionMethodBinding<>(
                        new SessionMethodRef<>("lobby", "default", "record", args, number),
                        (session, input) -> ((Game) session).record(input));
        var manager =
                new SessionManager(
                        process,
                        ticks,
                        Map.of("lobby/default", new SessionRegistration("lobby", () -> game)),
                        Map.of("lobby/default/record", binding));
        var controls = new ArrayList<SessionControl>();
        var host =
                TestHosts.detached(
                        new SessionHandler() {
                            @Override
                            public void create(SessionControl session) {
                                controls.add(session);
                                manager.create(session);
                            }

                            @Override
                            public void finish(SessionControl session) {
                                manager.finish(session);
                            }
                        });
        var spec = JvmSession.newBuilder().setSessionType("lobby/default").setCapacity(2).build();
        try {
            settle(ticks, host.create("session", spec));
            var control = controls.getFirst();
            var result = manager.method(control, call("record", "{\"message\":\"first\"}"));
            settle(ticks, result.toCompletableFuture());
            assertEquals("1", result.toCompletableFuture().join());
            assertEquals(List.of(Thread.currentThread()), game.threads);
            assertTrue(
                    manager.method(control, call("missing", "{}"))
                            .toCompletableFuture()
                            .isCompletedExceptionally());
            settle(ticks, host.finish("session", spec));
            var late = manager.method(control, call("record", "{\"message\":\"late\"}"));
            ticks.flush();
            assertTrue(late.toCompletableFuture().isCompletedExceptionally());
            assertEquals(1, game.threads.size());
        } finally {
            process.stop();
        }
    }

    private static SessionMethod call(String name, String argumentsJson) {
        return new SessionMethod(name, argumentsJson, null, () -> true);
    }

    private static void settle(TickExecutor ticks, CompletableFuture<?> future) {
        for (var index = 0; index < 10 && !future.isDone(); index++) ticks.flush();
        assertTrue(future.isDone());
    }

    public record Args(String message) {
        public Args {
            BackendValues.checkString(message);
        }
    }

    private static final class Game extends Session {
        final List<Thread> threads = new ArrayList<>();

        @Override
        public CompletableFuture<Void> onCreate(SessionScope scope) {
            scope.createInstance();
            return CompletableFuture.completedFuture(null);
        }

        long record(Args args) {
            threads.add(Thread.currentThread());
            return threads.size();
        }
    }
}
