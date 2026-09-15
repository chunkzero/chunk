package dev.chunkzero.runtime;

import static org.junit.jupiter.api.Assertions.*;

import chunk.v1.Common.DeploymentRef;
import chunk.v1.Common.PlayerRef;
import chunk.v1.Common.SessionRef;
import chunk.v1.SessionMethodsGrpc;
import chunk.v1.SessionMethodsOuterClass.SessionMethodCaller;
import chunk.v1.SessionMethodsOuterClass.SessionMethodPhase;
import chunk.v1.SessionMethodsOuterClass.SessionMethodRequest;
import chunk.v1.Supervision.ProcessIdentity;
import chunk.v1.Supervision.SessionCommand;

import dev.chunkzero.backend.api.BackendValues;
import dev.chunkzero.backend.api.JsonType;
import dev.chunkzero.backend.api.SessionMethodRef;
import dev.chunkzero.runtime.control.ProcessAuthentication;
import dev.chunkzero.runtime.minestom.internal.SessionMethodService;

import io.grpc.Metadata;
import io.grpc.Status;
import io.grpc.StatusRuntimeException;
import io.grpc.netty.shaded.io.grpc.netty.NettyChannelBuilder;
import io.grpc.netty.shaded.io.grpc.netty.NettyServerBuilder;
import io.grpc.stub.MetadataUtils;

import net.minestom.server.MinecraftServer;

import org.junit.jupiter.api.Test;

import tools.jackson.core.type.TypeReference;

import java.net.InetSocketAddress;
import java.util.Map;
import java.util.Objects;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;

class SessionMethodServiceTest {
    private static final String TOKEN = "test-process-token-with-at-least-32-characters";

    @Test
    void authenticatedCallsAreFencedQueuedDeduplicatedAndRetired() throws Exception {
        MinecraftServer.init();
        var ticks = new TickExecutor();
        var game = new Game();
        var manager =
                new SessionManager(
                        ticks,
                        Map.of("lobby/default", new SessionRegistration("lobby", () -> game)),
                        null);
        var command =
                SessionCommand.newBuilder()
                        .setOperationId("create")
                        .setSession(SessionRef.newBuilder().setId("session"))
                        .setGeneration(1)
                        .setSessionType("lobby/default")
                        .setCapacity(2)
                        .build();
        var created = manager.create(command);
        for (int i = 0; i < 4; i++) ticks.flush();
        created.join();
        var identity =
                ProcessIdentity.newBuilder()
                        .setDeployment(
                                DeploymentRef.newBuilder()
                                        .setEnvironment("env")
                                        .setDeployment("release"))
                        .setProcessId("process")
                        .setRuntimeId("runtime")
                        .setGeneration(1)
                        .setAppId("lobby")
                        .build();
        var caller =
                SessionMethodCaller.newBuilder()
                        .setDeliveryOperationId("delivery")
                        .setPlayer(PlayerRef.newBuilder().setId("player"))
                        .setMembershipGeneration(1)
                        .setOwnerGeneration(2)
                        .build();
        var live = new AtomicBoolean(true);
        var clock = new AtomicLong(100_000);
        var args = JsonType.of(new TypeReference<Args>() {}, Objects::requireNonNull);
        var number = JsonType.of(new TypeReference<Long>() {}, BackendValues::checkInteger);
        var binding =
                new SessionMethodBinding<>(
                        new SessionMethodRef<>("lobby", "default", "record", args, number),
                        (session, input) -> ((Game) session).record(input));
        var service =
                new SessionMethodService(
                        identity,
                        manager,
                        Map.of("lobby/default/record", binding),
                        request -> {
                            if (!live.get() || !request.getCaller().equals(caller))
                                throw new IllegalArgumentException("Stale caller");
                        },
                        clock::get);
        var server =
                NettyServerBuilder.forAddress(new InetSocketAddress("127.0.0.1", 0))
                        .intercept(new ProcessAuthentication(TOKEN))
                        .addService(service)
                        .build()
                        .start();
        var channel =
                NettyChannelBuilder.forAddress("127.0.0.1", server.getPort())
                        .usePlaintext()
                        .build();
        try {
            var headers = new Metadata();
            headers.put(
                    Metadata.Key.of("authorization", Metadata.ASCII_STRING_MARSHALLER),
                    "Bearer " + TOKEN);
            var unauthenticated =
                    SessionMethodsGrpc.newBlockingStub(channel)
                            .withDeadlineAfter(3, TimeUnit.SECONDS);
            var client =
                    SessionMethodsGrpc.newBlockingStub(channel)
                            .withInterceptors(MetadataUtils.newAttachHeadersInterceptor(headers));
            var request =
                    SessionMethodRequest.newBuilder()
                            .setIdentity(identity)
                            .setOperationId("process/1")
                            .setSequence(1)
                            .setSession(command.getSession())
                            .setSessionGeneration(1)
                            .setSessionType("lobby/default")
                            .setMethod("record")
                            .setArgumentsJson("{\"message\":\"first\"}")
                            .setCaller(caller)
                            .setIssuedAtMs(clock.get())
                            .setDeadlineMs(clock.get() + 30_000)
                            .build();
            assertEquals(
                    Status.Code.UNAUTHENTICATED,
                    assertThrows(StatusRuntimeException.class, () -> unauthenticated.call(request))
                            .getStatus()
                            .getCode());
            assertEquals(
                    Status.Code.PERMISSION_DENIED,
                    assertThrows(
                                    StatusRuntimeException.class,
                                    () ->
                                            client.call(
                                                    request.toBuilder()
                                                            .setIdentity(
                                                                    identity.toBuilder()
                                                                            .setGeneration(2))
                                                            .build()))
                            .getStatus()
                            .getCode());
            for (var invalid :
                    new SessionMethodRequest[] {
                        request.toBuilder().setSessionGeneration(2).build(),
                        request.toBuilder()
                                .setCaller(caller.toBuilder().setOwnerGeneration(3))
                                .build(),
                        request.toBuilder().setArgumentsJson("{\"message\":42}").build(),
                        request.toBuilder().setMethod("hidden").build()
                    }) {
                assertThrows(StatusRuntimeException.class, () -> client.call(invalid));
            }
            assertEquals(
                    SessionMethodPhase.SESSION_METHOD_PHASE_ACCEPTED,
                    client.call(request).getPhase());
            assertEquals(
                    SessionMethodPhase.SESSION_METHOD_PHASE_ACCEPTED,
                    client.call(request).getPhase());
            assertEquals(0, game.calls);
            ticks.flush();
            assertEquals(Thread.currentThread(), game.thread);
            assertEquals("1", client.call(request).getResultJson());
            assertEquals("1", client.call(request).getResultJson());
            assertEquals(1, game.calls);
            assertEquals(
                    Status.Code.ALREADY_EXISTS,
                    assertThrows(
                                    StatusRuntimeException.class,
                                    () ->
                                            client.call(
                                                    request.toBuilder()
                                                            .setArgumentsJson(
                                                                    "{\"message\":\"changed\"}")
                                                            .build()))
                            .getStatus()
                            .getCode());

            var departing = request.toBuilder().setOperationId("process/2").setSequence(2).build();
            assertEquals(
                    SessionMethodPhase.SESSION_METHOD_PHASE_ACCEPTED,
                    client.call(departing).getPhase());
            live.set(false);
            service.flush();
            ticks.flush();
            assertEquals(
                    SessionMethodPhase.SESSION_METHOD_PHASE_CANCELLED,
                    client.call(departing).getPhase());
            assertEquals(1, game.calls);
            live.set(true);

            var running = request.toBuilder().setOperationId("process/3").setSequence(3).build();
            game.started = new CountDownLatch(1);
            game.release = new CountDownLatch(1);
            assertEquals(
                    SessionMethodPhase.SESSION_METHOD_PHASE_ACCEPTED,
                    client.call(running).getPhase());
            var tick = Thread.ofPlatform().start(ticks::flush);
            try {
                assertTrue(game.started.await(3, TimeUnit.SECONDS));
                assertEquals(
                        SessionMethodPhase.SESSION_METHOD_PHASE_UNKNOWN,
                        client.cancel(running).getPhase());
            } finally {
                game.release.countDown();
                tick.join(3000);
            }
            assertFalse(tick.isAlive());
            assertEquals("2", client.call(running).getResultJson());
            assertEquals(2, game.calls);

            var expired =
                    request.toBuilder()
                            .setOperationId("process/5")
                            .setSequence(5)
                            .setIssuedAtMs(clock.get() - 2)
                            .setDeadlineMs(clock.get() - 1)
                            .build();
            assertEquals(
                    SessionMethodPhase.SESSION_METHOD_PHASE_UNKNOWN,
                    client.call(expired).getPhase());
            assertEquals(
                    SessionMethodPhase.SESSION_METHOD_PHASE_UNKNOWN,
                    client.call(expired.toBuilder().setDeadlineMs(clock.get() + 10).build())
                            .getPhase());

            for (int sequence = 10; sequence < 138; sequence++) {
                var queued =
                        request.toBuilder()
                                .setOperationId("process/" + sequence)
                                .setSequence(sequence)
                                .build();
                assertEquals(
                        SessionMethodPhase.SESSION_METHOD_PHASE_ACCEPTED,
                        client.call(queued).getPhase());
                assertEquals(
                        SessionMethodPhase.SESSION_METHOD_PHASE_CANCELLED,
                        client.cancel(queued).getPhase());
            }
            var overflow =
                    request.toBuilder().setOperationId("process/138").setSequence(138).build();
            assertEquals(
                    Status.Code.RESOURCE_EXHAUSTED,
                    assertThrows(StatusRuntimeException.class, () -> client.call(overflow))
                            .getStatus()
                            .getCode());
            ticks.flush();
            assertEquals(2, game.calls);

            var disposed =
                    request.toBuilder().setOperationId("process/200").setSequence(200).build();
            assertEquals(
                    SessionMethodPhase.SESSION_METHOD_PHASE_ACCEPTED,
                    client.call(disposed).getPhase());
            var ended = manager.finish(command);
            ticks.flush();
            assertEquals(
                    SessionMethodPhase.SESSION_METHOD_PHASE_CANCELLED,
                    client.call(disposed).getPhase());
            for (int i = 0; i < 8; i++) ticks.flush();
            ended.join();
            assertEquals(2, game.calls);
            clock.addAndGet(300_001);
            service.flush();
            var refreshed =
                    request.toBuilder()
                            .setIssuedAtMs(clock.get())
                            .setDeadlineMs(clock.get() + 30_000)
                            .build();
            assertEquals(
                    SessionMethodPhase.SESSION_METHOD_PHASE_UNKNOWN,
                    client.call(refreshed).getPhase());
            assertEquals(2, game.calls);
        } finally {
            if (game.release != null) game.release.countDown();
            service.close();
            channel.shutdownNow().awaitTermination(3, TimeUnit.SECONDS);
            server.shutdownNow().awaitTermination(3, TimeUnit.SECONDS);
            MinecraftServer.process().stop();
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
        CountDownLatch started;
        CountDownLatch release;

        @Override
        public CompletableFuture<Void> onCreate(SessionScope scope) {
            scope.createInstance();
            return CompletableFuture.completedFuture(null);
        }

        long record(Args args) {
            thread = Thread.currentThread();
            if (started != null) {
                started.countDown();
                try {
                    if (!release.await(3, TimeUnit.SECONDS))
                        throw new IllegalStateException("Timed out");
                } catch (InterruptedException error) {
                    Thread.currentThread().interrupt();
                    throw new IllegalStateException(error);
                }
            }
            calls++;
            return calls;
        }
    }
}
