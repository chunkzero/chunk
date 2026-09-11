package dev.chunkzero.runtime;

import static org.junit.jupiter.api.Assertions.*;

import chunk.v1.BackendGrpc;
import chunk.v1.BackendOuterClass.BackendMutation;
import chunk.v1.BackendOuterClass.BackendQuery;
import chunk.v1.BackendOuterClass.BackendResult;
import chunk.v1.BackendOuterClass.BackendUpdate;
import chunk.v1.BackendOuterClass.BackendWatchGroup;
import chunk.v1.Common.SessionRef;
import chunk.v1.GameplayOuterClass.PlayerDelivery;
import chunk.v1.Supervision.SessionCommand;

import com.google.protobuf.ByteString;

import dev.chunkzero.backend.api.BackendJson;
import dev.chunkzero.backend.api.SessionId;
import dev.chunkzero.backend.client.BackendSession;
import dev.chunkzero.backend.client.SessionIdentity;
import dev.chunkzero.example.ExampleSessions;

import io.grpc.ManagedChannelBuilder;
import io.grpc.ServerBuilder;
import io.grpc.stub.StreamObserver;

import net.minestom.server.MinecraftServer;
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
        MinecraftServer.init();
        var ticks = new TickExecutor();
        var coins = new LinkedBlockingQueue<BackendMutation>();
        var server =
                ServerBuilder.forPort(0)
                        .addService(
                                new BackendGrpc.BackendImplBase() {
                                    @Override
                                    public void query(
                                            BackendQuery request,
                                            StreamObserver<BackendResult> response) {
                                        assertEquals("shared/players/stats", request.getFunction());
                                        response.onNext(
                                                BackendResult.newBuilder()
                                                        .setRevision(1)
                                                        .setResultJson(
                                                                ByteString.copyFromUtf8(
                                                                        "{\"coins\":0,\"visits\":0}"))
                                                        .build());
                                        response.onCompleted();
                                    }

                                    @Override
                                    public void mutate(
                                            BackendMutation request,
                                            StreamObserver<BackendResult> response) {
                                        response.onNext(
                                                BackendResult.newBuilder()
                                                        .setRevision(1)
                                                        .setResultJson(ByteString.copyFromUtf8("1"))
                                                        .build());
                                        response.onCompleted();
                                        if (request.getFunction().equals("shared/players/coin"))
                                            coins.add(request);
                                    }

                                    @Override
                                    public void watchGroup(
                                            BackendWatchGroup request,
                                            StreamObserver<BackendUpdate> response) {
                                        response.onNext(
                                                BackendUpdate.newBuilder()
                                                        .setRevision(1)
                                                        .addResultsJson(
                                                                ByteString.copyFromUtf8(
                                                                        "{\"coins\":0,\"visits\":0}"))
                                                        .addErrors("")
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
                        ticks,
                        Map.of(
                                "lobby",
                                        new SessionRegistration(
                                                "lobby", ExampleSessions.INSTANCE::lobby),
                                "arena",
                                        new SessionRegistration(
                                                "arena", ExampleSessions.INSTANCE::arena)),
                        (session, app) ->
                                new BackendSession(
                                        channel,
                                        "test-credential-with-at-least-32-bytes",
                                        "test",
                                        "build",
                                        new SessionIdentity(
                                                new SessionId(session), app, Optional.empty()),
                                        scheduler,
                                        Duration.ofSeconds(5)));
        var lobby = session("lobby");
        var arena = session("arena");
        try {
            await(ticks, manager.create(lobby));
            await(ticks, manager.create(arena));
            var uuid = UUID.randomUUID();
            long generation = 0;
            for (var destination : List.of("lobby", "arena", "arena")) {
                generation++;
                var connection =
                        new PlayerConnection() {
                            @Override
                            public void sendPacket(SendablePacket packet) {}

                            @Override
                            public SocketAddress getRemoteAddress() {
                                return new InetSocketAddress(0);
                            }
                        };
                connection.setClientState(ConnectionState.PLAY);
                var player = new ManagedPlayer(connection, new GameProfile(uuid, "player"));
                player.setBinding(
                        PlayerDelivery.newBuilder().setOwnerGeneration(generation).build());
                var managed = manager.get(destination, 1);
                try {
                    await(ticks, managed.join(player));
                    var result =
                            CompletableFuture.supplyAsync(
                                            () ->
                                                    MinecraftServer.getCommandManager()
                                                            .execute(player, "coin"))
                                    .get(5, TimeUnit.SECONDS);
                    assertEquals(CommandResult.Type.SUCCESS, result.getType());
                    var call = pump(ticks, coins::poll);
                    var caller = BackendJson.mapper().readTree(call.getCallerJson().toStringUtf8());
                    assertEquals(uuid.toString(), caller.get("player").asString());
                    assertEquals(destination, caller.get("session").asString());
                    assertEquals(
                            destination + "/" + uuid + "/" + generation + "/coin-0",
                            call.getOperationId());
                } finally {
                    await(ticks, managed.leave(player));
                    connection.disconnect();
                }
            }
        } finally {
            try {
                await(ticks, manager.finish(lobby));
                await(ticks, manager.finish(arena));
            } finally {
                channel.shutdownNow().awaitTermination(3, TimeUnit.SECONDS);
                server.shutdownNow().awaitTermination(3, TimeUnit.SECONDS);
                scheduler.shutdownNow();
                scheduler.awaitTermination(3, TimeUnit.SECONDS);
                MinecraftServer.process().stop();
            }
        }
    }

    private static SessionCommand session(String id) {
        return SessionCommand.newBuilder()
                .setOperationId(id)
                .setSession(SessionRef.newBuilder().setId(id))
                .setGeneration(1)
                .setSessionType(id)
                .setCapacity(2)
                .build();
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
