package dev.chunkzero.runtime;

import static org.junit.jupiter.api.Assertions.*;

import chunk.sync.v1.Jvm.JvmMethodCall;
import chunk.sync.v1.Jvm.JvmMethodPhase;
import chunk.sync.v1.Jvm.JvmSession;
import chunk.v1.Supervision.SessionPhase;

import com.google.protobuf.ByteString;

import dev.chunkzero.backend.api.BackendValues;
import dev.chunkzero.backend.api.JsonType;
import dev.chunkzero.backend.api.SessionMethodRef;
import dev.chunkzero.runtime.minestom.internal.GameplayService;
import dev.chunkzero.runtime.minestom.internal.ProcessService;
import dev.chunkzero.runtime.minestom.internal.SessionMethodService;

import net.minestom.server.ServerProcess;

import org.junit.jupiter.api.Test;

import tools.jackson.core.type.TypeReference;

import java.util.Map;
import java.util.Objects;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.atomic.AtomicBoolean;

class SessionMethodServiceTest {
    @Test
    void topicMethodsRunOnceOnTheTickThreadUnlessCancelledOrTheirPlayerLeft() throws Exception {
        var process = ServerProcess.create();
        var ticks = new TickExecutor();
        var game = new Game();
        var manager =
                new SessionManager(
                        process,
                        ticks,
                        Map.of("lobby/default", new SessionRegistration("lobby", () -> game)),
                        null);
        var args = JsonType.of(new TypeReference<Args>() {}, Objects::requireNonNull);
        var number = JsonType.of(new TypeReference<Long>() {}, BackendValues::checkInteger);
        var binding =
                new SessionMethodBinding<>(
                        new SessionMethodRef<>("lobby", "default", "record", args, number),
                        (session, input) -> ((Game) session).record(input));
        var arrived = new AtomicBoolean(true);
        var gameplay = new GameplayService(manager, System::nanoTime, () -> true);
        try (var core = new FakeCore()) {
            var methods =
                    new SessionMethodService(
                            manager,
                            Map.of("lobby/default/record", binding),
                            (delivery, session) ->
                                    arrived.get()
                                            && delivery.equals("delivery")
                                            && session.equals("session"),
                            core::methodResult,
                            System::currentTimeMillis);
            core.connect(new ProcessService(manager, () -> true, gameplay, methods));
            core.put(
                    "session/session",
                    JvmSession.newBuilder().setSessionType("lobby/default").setCapacity(2).build());
            while (manager.phase("session") != SessionPhase.SESSION_PHASE_READY) {
                ticks.flush();
                Thread.sleep(10);
            }
            var call =
                    JvmMethodCall.newBuilder()
                            .setSession("session")
                            .setMethod("record")
                            .setArgumentsJson(ByteString.copyFromUtf8("{\"message\":\"first\"}"))
                            .setDelivery("delivery")
                            .setDeadlineMs(System.currentTimeMillis() + 30_000)
                            .build();

            core.put("method/completes", call);
            var completed = core.result("completes", ticks::flush);
            assertEquals(JvmMethodPhase.JVM_METHOD_PHASE_COMPLETED, completed.getPhase());
            assertEquals("1", completed.getResultJson().toStringUtf8());
            assertEquals(Thread.currentThread(), game.thread);

            core.put("method/cancelled", call.toBuilder().setCancel(true).build());
            assertEquals(
                    JvmMethodPhase.JVM_METHOD_PHASE_CANCELLED,
                    core.result("cancelled", ticks::flush).getPhase());

            arrived.set(false);
            core.put("method/departed", call);
            assertEquals(
                    JvmMethodPhase.JVM_METHOD_PHASE_CANCELLED,
                    core.result("departed", ticks::flush).getPhase());
            assertEquals(1, game.calls);
        } finally {
            gameplay.close();
            process.stop();
        }
    }

    public record Args(String message) {
        public Args {
            BackendValues.checkString(message);
        }
    }

    private static final class Game extends Session {
        int calls;
        Thread thread;

        @Override
        public CompletableFuture<Void> onCreate(SessionScope scope) {
            scope.createInstance();
            return CompletableFuture.completedFuture(null);
        }

        long record(Args args) {
            thread = Thread.currentThread();
            return ++calls;
        }
    }
}
