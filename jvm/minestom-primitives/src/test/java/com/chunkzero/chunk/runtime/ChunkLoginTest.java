package com.chunkzero.chunk.runtime;

import static org.junit.jupiter.api.Assertions.*;

import chunk.sync.v1.CoreOuterClass.Position;
import chunk.sync.v1.Gateway.PlayerIdentity;
import chunk.sync.v1.Gateway.PlayerProperty;
import chunk.sync.v1.Jvm.JvmDelivery;
import chunk.sync.v1.Jvm.JvmDeliveryPhase;
import chunk.sync.v1.Jvm.JvmSession;
import chunk.sync.v1.Jvm.PlayerSetup;

import com.chunkzero.chunk.minestom.ChunkLogin;
import com.google.protobuf.ByteString;

import net.minestom.server.MinecraftServer;
import net.minestom.server.network.ConnectionState;
import net.minestom.server.network.NetworkBuffer;
import net.minestom.server.network.packet.PacketVanilla;
import net.minestom.server.network.packet.client.handshake.ClientHandshakePacket;
import net.minestom.server.network.packet.client.login.ClientLoginPluginResponsePacket;
import net.minestom.server.network.packet.client.login.ClientLoginStartPacket;
import net.minestom.server.network.packet.server.ServerPacket;
import net.minestom.server.network.packet.server.login.LoginDisconnectPacket;
import net.minestom.server.network.packet.server.login.LoginPluginRequestPacket;
import net.minestom.server.network.packet.server.login.LoginSuccessPacket;

import org.junit.jupiter.api.Test;

import java.io.IOException;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.util.ArrayList;
import java.util.Map;
import java.util.TreeMap;
import java.util.UUID;
import java.util.concurrent.TimeUnit;

class ChunkLoginTest {
    private final UUID uuid = UUID.randomUUID();
    private final Map<String, ByteString> topic = new TreeMap<>();

    @Test
    void upstreamMinestomAdmitsOnlyDeliveredPlayersAndReleasesThemOnceDisconnected()
            throws Exception {
        MinecraftServer.setCompressionThreshold(0);
        var server = MinecraftServer.init();
        var host =
                ChunkSessions.detached(
                        new SessionHandler() {
                            @Override
                            public void create(SessionControl session) {
                                session.ready();
                            }

                            @Override
                            public void finish(SessionControl session) {
                                session.ended();
                            }
                        },
                        session -> null);
        var sockets = new ArrayList<Socket>();
        try (var login = ChunkLogin.create(host)) {
            MinecraftServer.getGlobalEventHandler().addChild(login.events());
            server.start(new InetSocketAddress("127.0.0.1", 0));
            var port = MinecraftServer.getServer().getPort();
            topic.put(
                    "session/game",
                    JvmSession.newBuilder()
                            .setSessionType("app/default")
                            .setCapacity(4)
                            .build()
                            .toByteString());
            var first = prepare(host, "first", 1);
            for (var invalid :
                    java.util.List.of(
                            first.toBuilder().setCapability(ByteString.EMPTY).build(),
                            first.toBuilder().setOperationId("unknown").build())) {
                var socket = login(port, "player", invalid);
                sockets.add(socket);
                assertInstanceOf(LoginDisconnectPacket.class, packet(socket));
            }
            var other = login(port, "other", first);
            sockets.add(other);
            assertInstanceOf(LoginDisconnectPacket.class, packet(other));

            var admitted = login(port, "player", first);
            sockets.add(admitted);
            var success = assertInstanceOf(LoginSuccessPacket.class, packet(admitted));
            assertEquals("signature", success.gameProfile().properties().getFirst().signature());
            assertEquals(JvmDeliveryPhase.JVM_DELIVERY_PHASE_ATTACHED, phase(host, "first"));

            // Withdrawal disconnects the player, who is released once their connection closed.
            topic.put(
                    "delivery/first",
                    delivery(1).toBuilder().setWithdraw(true).build().toByteString());
            host.apply(topic);
            await(host, "first", JvmDeliveryPhase.JVM_DELIVERY_PHASE_CLOSED);
            // The release lets a newer delivery admit the same player.
            var next = prepare(host, "next", 2);
            var again = login(port, "player", next);
            sockets.add(again);
            assertInstanceOf(LoginSuccessPacket.class, packet(again));
        } finally {
            for (var socket : sockets) socket.close();
            host.close();
            MinecraftServer.stopCleanly();
        }
    }

    private PlayerSetup prepare(ChunkSessions host, String operation, long revision) {
        topic.put("delivery/" + operation, delivery(revision).toByteString());
        host.apply(topic);
        // A delivery listed with its session waits for the next sweep.
        host.sweep();
        var status =
                host.inventory().getDeliveriesList().stream()
                        .filter(delivery -> delivery.getOperationId().equals(operation))
                        .findFirst()
                        .orElseThrow();
        assertEquals(JvmDeliveryPhase.JVM_DELIVERY_PHASE_PREPARED, status.getPhase());
        return PlayerSetup.newBuilder()
                .setOperationId(operation)
                .setCapability(status.getCapability())
                .build();
    }

    private JvmDelivery delivery(long revision) {
        return JvmDelivery.newBuilder()
                .setSession("game")
                .setGeneration(Position.newBuilder().setEpoch(1).setRevision(revision))
                .setPlayer(
                        PlayerIdentity.newBuilder()
                                .setUuid(uuid.toString())
                                .setUsername("player")
                                .addProperties(
                                        PlayerProperty.newBuilder()
                                                .setName("textures")
                                                .setValue("value")
                                                .setSignature("signature")))
                .build();
    }

    private static JvmDeliveryPhase phase(ChunkSessions host, String operation) {
        return host.inventory().getDeliveriesList().stream()
                .filter(delivery -> delivery.getOperationId().equals(operation))
                .findFirst()
                .orElseThrow()
                .getPhase();
    }

    private static void await(ChunkSessions host, String operation, JvmDeliveryPhase phase)
            throws InterruptedException {
        var deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(10);
        while (phase(host, operation) != phase) {
            assertTrue(System.nanoTime() < deadline, operation + " never reached " + phase);
            Thread.sleep(10);
        }
    }

    /** Starts a login on {@code port}, answering the delivery request with {@code setup}. */
    private Socket login(int port, String name, PlayerSetup setup) throws IOException {
        var socket = new Socket("127.0.0.1", port);
        socket.setSoTimeout(10_000);
        send(
                socket,
                0,
                ClientHandshakePacket.SERIALIZER,
                new ClientHandshakePacket(
                        MinecraftServer.PROTOCOL_VERSION,
                        "localhost",
                        25565,
                        ClientHandshakePacket.Intent.LOGIN));
        send(socket, 0, ClientLoginStartPacket.SERIALIZER, new ClientLoginStartPacket(name, uuid));
        var challenge = assertInstanceOf(LoginPluginRequestPacket.class, packet(socket));
        assertEquals("chunk:delivery", challenge.channel());
        send(
                socket,
                2,
                ClientLoginPluginResponsePacket.SERIALIZER,
                new ClientLoginPluginResponsePacket(challenge.messageId(), setup.toByteArray()));
        return socket;
    }

    private static <T> void send(Socket socket, int id, NetworkBuffer.Type<T> type, T packet)
            throws IOException {
        var body =
                NetworkBuffer.makeArray(
                        buffer -> {
                            buffer.write(NetworkBuffer.VAR_INT, id);
                            buffer.write(type, packet);
                        });
        socket.getOutputStream()
                .write(
                        NetworkBuffer.makeArray(
                                buffer -> {
                                    buffer.write(NetworkBuffer.VAR_INT, body.length);
                                    buffer.write(NetworkBuffer.RAW_BYTES, body);
                                }));
    }

    private static ServerPacket packet(Socket socket) throws IOException {
        var input = socket.getInputStream();
        var length = 0;
        for (var shift = 0; ; shift += 7) {
            var next = input.read();
            if (next < 0 || shift >= 21) throw new IOException("Truncated frame");
            length |= (next & 127) << shift;
            if ((next & 128) == 0) break;
        }
        var bytes = input.readNBytes(length);
        var buffer = NetworkBuffer.wrap(bytes, 0, bytes.length);
        return PacketVanilla.SERVER_PACKET_PARSER.parse(
                ConnectionState.LOGIN, buffer.read(NetworkBuffer.VAR_INT), buffer);
    }
}
