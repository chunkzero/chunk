package com.chunkzero.chunk.runtime;

import static org.junit.jupiter.api.Assertions.*;

import chunk.sync.v1.CoreGrpc;
import chunk.sync.v1.CoreOuterClass.CallRequest;
import chunk.sync.v1.CoreOuterClass.CallResponse;
import chunk.sync.v1.CoreOuterClass.Entry;
import chunk.sync.v1.CoreOuterClass.Position;
import chunk.sync.v1.CoreOuterClass.SubscribeRequest;
import chunk.sync.v1.CoreOuterClass.Update;
import chunk.sync.v1.Gateway.PlayerIdentity;
import chunk.sync.v1.Jvm.JvmDelivery;
import chunk.sync.v1.Jvm.JvmSession;

import com.chunkzero.chunk.backend.api.SessionId;
import com.chunkzero.chunk.backend.client.BackendSession;
import com.chunkzero.chunk.backend.client.SessionIdentity;
import com.chunkzero.chunk.example.ExampleSessions;
import com.google.protobuf.ByteString;

import io.grpc.ManagedChannelBuilder;
import io.grpc.ServerBuilder;
import io.grpc.stub.StreamObserver;

import net.minestom.server.ServerProcess;
import net.minestom.server.command.builder.CommandResult;
import net.minestom.server.network.ConnectionState;
import net.minestom.server.network.packet.server.SendablePacket;
import net.minestom.server.network.player.GameProfile;
import net.minestom.server.network.player.PlayerConnection;

import org.junit.jupiter.api.Test;

import java.net.InetSocketAddress;
import java.net.SocketAddress;
import java.time.Duration;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.UUID;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.Executors;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;
import java.util.function.Supplier;

class CoinCommandTest {
    @Test
    void commandsReachTheBackendFromPlayerThreadsAfterMovesAndRejoins() throws Exception {
        var process = ServerProcess.create();
        ExampleSessions.INSTANCE.register(process);
        var ticks = new TickExecutor();
        var coins = new LinkedBlockingQueue<CallRequest>();
        var stats = ByteString.copyFromUtf8("{\"coins\":0,\"visits\":0}");
        var server =
                ServerBuilder.forPort(0)
                        .addService(
                                new CoreGrpc.CoreImplBase() {
                                    @Override
                                    public void call(
                                            CallRequest request,
                                            StreamObserver<CallResponse> response) {
                                        var query = request.getOperationId().isEmpty();
                                        if (query)
                                            assertEquals(
                                                    "shared/players/stats", request.getMethod());
                                        response.onNext(
                                                CallResponse.newBuilder()
                                                        .setPosition(at(1))
                                                        .setResult(
                                                                query
                                                                        ? stats
                                                                        : ByteString.copyFromUtf8(
                                                                                "1"))
                                                        .build());
                                        response.onCompleted();
                                        if (request.getMethod().equals("shared/players/coin"))
                                            coins.add(request);
                                    }

                                    @Override
                                    public void subscribe(
                                            SubscribeRequest request,
                                            StreamObserver<Update> response) {
                                        response.onNext(
                                                Update.newBuilder()
                                                        .setPosition(at(1))
                                                        .setSnapshot(true)
                                                        .addUpserts(
                                                                Entry.newBuilder()
                                                                        .setKey("0")
                                                                        .setValue(stats))
                                                        .build());
                                    }
                                })
                        .build()
                        .start();
        var channel =
                ManagedChannelBuilder.forAddress("127.0.0.1", server.getPort())
                        .usePlaintext()
                        .build();
        var scheduler = Executors.newSingleThreadScheduledExecutor();
        var manager =
                new SessionManager(
                        process,
                        ticks,
                        Map.of(
                                "lobby",
                                        new SessionRegistration(
                                                "lobby", ExampleSessions.INSTANCE::lobby),
                                "arena",
                                        new SessionRegistration(
                                                "arena",
                                                () -> ExampleSessions.INSTANCE.arena("Arena"))),
                        Map.of());
        var host =
                ChunkSessions.detached(
                        manager,
                        session ->
                                BackendSession.overCore(
                                        channel,
                                        "test-credential-with-at-least-32-bytes",
                                        "build",
                                        new SessionIdentity(
                                                new SessionId(session), session, Optional.empty()),
                                        scheduler,
                                        Duration.ofSeconds(5)));
        var lobby = session("lobby");
        var arena = session("arena");
        try {
            await(ticks, host.create("lobby", lobby));
            await(ticks, host.create("arena", arena));
            var uuid = UUID.randomUUID();
            long generation = 0;
            for (var destination : List.of("lobby", "arena", "arena")) {
                generation++;
                var connection =
                        new PlayerConnection(process) {
                            @Override
                            public void sendPacket(SendablePacket packet) {}

                            @Override
                            public SocketAddress getRemoteAddress() {
                                return new InetSocketAddress(0);
                            }
                        };
                connection.setClientState(ConnectionState.PLAY);
                var player = new ManagedPlayer(connection, new GameProfile(uuid, "player"));
                player.setDelivery(
                        new Delivery(
                                null,
                                "delivery",
                                JvmDelivery.newBuilder()
                                        .setSession(destination)
                                        .setGeneration(at(generation))
                                        .setPlayer(
                                                PlayerIdentity.newBuilder()
                                                        .setUuid(uuid.toString())
                                                        .setUsername("player"))
                                        .build(),
                                0));
                var managed = manager.get(destination);
                try {
                    await(ticks, managed.join(player));
                    var result =
                            CompletableFuture.supplyAsync(
                                            () -> process.commandManager().execute(player, "coin"))
                                    .get(5, TimeUnit.SECONDS);
                    assertEquals(CommandResult.Type.SUCCESS, result.getType());
                    var call = pump(ticks, coins::poll);
                    assertEquals(uuid.toString(), call.getCaller().getPlayer());
                    assertEquals(destination, call.getCaller().getSession());
                    assertEquals(
                            destination + "/" + uuid + "/1." + generation + "/coin-0",
                            call.getOperationId());
                } finally {
                    await(ticks, managed.leave(player));
                    connection.disconnect();
                }
            }
        } finally {
            try {
                await(ticks, host.finish("lobby", lobby));
                await(ticks, host.finish("arena", arena));
            } finally {
                channel.shutdownNow().awaitTermination(3, TimeUnit.SECONDS);
                server.shutdownNow().awaitTermination(3, TimeUnit.SECONDS);
                scheduler.shutdownNow();
                scheduler.awaitTermination(3, TimeUnit.SECONDS);
                process.stop();
            }
        }
    }

    private static JvmSession session(String type) {
        return JvmSession.newBuilder().setSessionType(type).setCapacity(2).build();
    }

    private static Position at(long revision) {
        return Position.newBuilder().setEpoch(1).setRevision(revision).build();
    }

    private static void await(TickExecutor ticks, CompletableFuture<?> future) throws Exception {
        pump(ticks, () -> future.isDone() ? true : null);
        future.get();
    }

    private static <T> T pump(TickExecutor ticks, Supplier<T> next) throws InterruptedException {
        var deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        while (System.nanoTime() < deadline) {
            ticks.flush();
            var result = next.get();
            if (result != null) return result;
            Thread.sleep(1);
        }
        throw new AssertionError("Expected session or command completion did not arrive");
    }
}
