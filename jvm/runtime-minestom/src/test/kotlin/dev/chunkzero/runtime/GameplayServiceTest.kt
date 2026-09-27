package dev.chunkzero.runtime

import chunk.v1.Common.DeploymentRef
import chunk.v1.Common.Identity
import chunk.v1.Common.PlayerRef
import chunk.v1.Common.Property
import chunk.v1.Common.SessionRef
import chunk.v1.GameplayGrpc
import chunk.v1.GameplayOuterClass.ConfigurationRequest
import chunk.v1.GameplayOuterClass.PlayerDelivery
import chunk.v1.GameplayOuterClass.PlayerSetup
import com.google.protobuf.ByteString
import dev.chunkzero.runtime.bootstrap.FlatSession
import dev.chunkzero.runtime.minestom.internal.GameplayService
import io.grpc.Status
import io.grpc.StatusRuntimeException
import io.grpc.netty.shaded.io.grpc.netty.NettyChannelBuilder
import io.grpc.netty.shaded.io.grpc.netty.NettyServerBuilder
import net.kyori.adventure.text.Component
import net.minestom.server.MinecraftConstants
import net.minestom.server.ServerProcess
import net.minestom.server.network.ConnectionState
import net.minestom.server.network.NetworkBuffer
import net.minestom.server.network.packet.PacketVanilla
import net.minestom.server.network.packet.client.common.ClientSettingsPacket
import net.minestom.server.network.packet.client.configuration.ClientFinishConfigurationPacket
import net.minestom.server.network.packet.client.configuration.ClientSelectKnownPacksPacket
import net.minestom.server.network.packet.client.handshake.ClientHandshakePacket
import net.minestom.server.network.packet.client.login.ClientLoginAcknowledgedPacket
import net.minestom.server.network.packet.client.login.ClientLoginPluginResponsePacket
import net.minestom.server.network.packet.client.login.ClientLoginStartPacket
import net.minestom.server.network.packet.server.ServerPacket
import net.minestom.server.network.packet.server.common.DisconnectPacket
import net.minestom.server.network.packet.server.configuration.FinishConfigurationPacket
import net.minestom.server.network.packet.server.configuration.RegistryDataPacket
import net.minestom.server.network.packet.server.configuration.SelectKnownPacksPacket
import net.minestom.server.network.packet.server.login.LoginDisconnectPacket
import net.minestom.server.network.packet.server.login.LoginPluginRequestPacket
import net.minestom.server.network.packet.server.login.LoginSuccessPacket
import net.minestom.server.network.packet.server.play.JoinGamePacket
import net.minestom.server.network.player.ClientSettings
import net.minestom.server.registry.Registries
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import java.net.InetSocketAddress
import java.net.Socket
import java.util.UUID
import java.util.concurrent.TimeUnit
import java.util.function.Supplier

class GameplayServiceTest {
    @Test
    fun `authenticated configuration and delivery reject incompatible or replayed input`() {
        val minecraft = ServerProcess.create()
        minecraft.setCompressionThreshold(0)
        val deployment =
            DeploymentRef
                .newBuilder()
                .setEnvironment("local")
                .setDeployment("build-a")
                .build()
        minecraft.connectionManager().setPlayerProvider(::ManagedPlayer)
        val ticks = TickExecutor()
        val manager = SessionManager(minecraft, ticks, mapOf("bridge" to Supplier { FlatSession() }))
        manager.create(
            chunk.v1.Supervision.SessionCommand
                .newBuilder()
                .setOperationId(
                    "fixture",
                ).setSession(
                    SessionRef.newBuilder().setId("bridge"),
                ).setGeneration(1)
                .setSessionType("bridge")
                .setCapacity(128)
                .build(),
        )
        repeat(3) { ticks.flush() }
        val clock =
            java.util.concurrent.atomic
                .AtomicLong(System.nanoTime())
        val service = GameplayService(deployment, 7, manager, clock::get, "bridge")
        minecraft
            .schedulerManager()
            .buildTask {
                ticks.flush()
            }.repeat(
                net.minestom.server.timer.TaskSchedule
                    .tick(1),
            ).schedule()
        minecraft.start(InetSocketAddress("127.0.0.1", 0))
        service.endpoint = "127.0.0.1:${minecraft.server().port}"
        val server =
            NettyServerBuilder
                .forAddress(InetSocketAddress("127.0.0.1", 0))
                .addService(service)
                .build()
                .start()
        val channel = NettyChannelBuilder.forAddress("127.0.0.1", server.port).usePlaintext().build()
        try {
            val request = ConfigurationRequest.newBuilder().setDeployment(deployment).build()
            val stub = GameplayGrpc.newBlockingStub(channel).withDeadlineAfter(3, TimeUnit.SECONDS)
            val configuration = stub.configuration(request)
            assertEquals(MinecraftConstants.PROTOCOL_VERSION, configuration.protocol)
            assertEquals(7, configuration.processGeneration)
            assertEquals(deployment, configuration.deployment)
            assertEquals(
                Status.Code.PERMISSION_DENIED,
                assertThrows(StatusRuntimeException::class.java) {
                    stub.configuration(
                        request.toBuilder().setDeployment(deployment.toBuilder().setDeployment("other")).build(),
                    )
                }.status.code,
            )
            val delivery =
                PlayerDelivery
                    .newBuilder()
                    .setDeployment(deployment)
                    .setProcessGeneration(7)
                    .setRuntimeId("bridge")
                    .setOwnerGeneration(1)
                    .setSessionGeneration(1)
                    .setMembershipGeneration(1)
                    .setProxyId("test-proxy")
                    .setConnectionId("test-connection")
                    .setOperationId("delivery-1")
                    .setSession(SessionRef.newBuilder().setId("bridge"))
                    .setPlayer(PlayerRef.newBuilder().setId("player"))
                    .setIdentity(
                        Identity
                            .newBuilder()
                            .setUuid(UUID.randomUUID().toString())
                            .setUsername("player")
                            .addProperties(
                                Property
                                    .newBuilder()
                                    .setName("textures")
                                    .setValue("value")
                                    .setSignature("signature"),
                            ),
                    ).setProtocol(MinecraftConstants.PROTOCOL_VERSION)
                    .build()
            for (invalid in listOf(
                delivery.toBuilder().setProcessGeneration(6).build(),
                delivery.toBuilder().setProtocol(MinecraftConstants.PROTOCOL_VERSION - 1).build(),
                delivery.toBuilder().setDeployment(deployment.toBuilder().setDeployment("other")).build(),
                delivery.toBuilder().setProxyId("\u00a0").build(),
            )) {
                assertEquals(
                    Status.Code.FAILED_PRECONDITION,
                    assertThrows(StatusRuntimeException::class.java) {
                        stub.preparePlayer(invalid)
                    }.status.code,
                )
            }
            val prepared = stub.preparePlayer(delivery)
            assertEquals(prepared, stub.preparePlayer(delivery))
            assertEquals(
                Status.Code.FAILED_PRECONDITION,
                assertThrows(StatusRuntimeException::class.java) {
                    stub.preparePlayer(delivery.toBuilder().setOwnerGeneration(2).build())
                }.status.code,
            )
            val setup =
                PlayerSetup
                    .newBuilder()
                    .setOperationId(
                        delivery.operationId,
                    ).setCapability(prepared.capability)
                    .build()

            fun attempt(
                payload: PlayerSetup,
                name: String = "player",
            ): Socket {
                val socket = Socket("127.0.0.1", minecraft.server().port)
                socket.soTimeout = 5000
                socket.send(
                    0,
                    ClientHandshakePacket.SERIALIZER,
                    ClientHandshakePacket(
                        MinecraftConstants.PROTOCOL_VERSION,
                        "localhost",
                        25565,
                        ClientHandshakePacket.Intent.LOGIN,
                    ),
                )
                socket.send(
                    0,
                    ClientLoginStartPacket.SERIALIZER,
                    ClientLoginStartPacket(name, UUID.fromString(delivery.identity.uuid)),
                )
                val challenge = socket.packet(ConnectionState.LOGIN) as LoginPluginRequestPacket
                assertEquals("chunk:delivery", challenge.channel())
                socket.send(
                    2,
                    ClientLoginPluginResponsePacket.SERIALIZER,
                    ClientLoginPluginResponsePacket(challenge.messageId(), payload.toByteArray()),
                )
                return socket
            }
            for (invalid in listOf(
                setup.toBuilder().setCapability(ByteString.EMPTY).build(),
                setup.toBuilder().setOperationId("unknown").build(),
            )) {
                attempt(invalid).use { assertTrue(it.packet(ConnectionState.LOGIN) is LoginDisconnectPacket) }
            }
            attempt(setup, "other").use { assertTrue(it.packet(ConnectionState.LOGIN) is LoginDisconnectPacket) }
            assertTrue(minecraft.connectionManager().onlinePlayers.isEmpty())
            attempt(setup).use { socket ->
                val success = socket.packet(ConnectionState.LOGIN) as LoginSuccessPacket
                assertEquals(delivery.identity.uuid, success.gameProfile().uuid().toString())
                assertEquals(
                    "signature",
                    success
                        .gameProfile()
                        .properties()
                        .single()
                        .signature(),
                )
                attempt(setup).use { assertTrue(it.packet(ConnectionState.LOGIN) is LoginDisconnectPacket) }
                socket.send(3, ClientLoginAcknowledgedPacket.SERIALIZER, ClientLoginAcknowledgedPacket())
                socket.send(0, ClientSettingsPacket.SERIALIZER, ClientSettingsPacket(ClientSettings.DEFAULT))
                var registries = 0
                while (true) {
                    when (socket.packet(ConnectionState.CONFIGURATION)) {
                        is SelectKnownPacksPacket -> {
                            socket.send(
                                7,
                                ClientSelectKnownPacksPacket.SERIALIZER,
                                ClientSelectKnownPacksPacket(emptyList()),
                            )
                        }

                        is RegistryDataPacket -> {
                            registries++
                        }

                        is FinishConfigurationPacket -> {
                            break
                        }

                        else -> {}
                    }
                }
                assertTrue(registries > 0)
                socket.send(3, ClientFinishConfigurationPacket.SERIALIZER, ClientFinishConfigurationPacket())
                assertTrue(socket.packet(ConnectionState.PLAY) is JoinGamePacket)
                val player =
                    requireNotNull(
                        minecraft.connectionManager().getOnlinePlayerByUuid(
                            UUID.fromString(delivery.identity.uuid),
                        ),
                    )
                player.kick(Component.text("review kick reason"))
                while (true) {
                    val packet = socket.packet(ConnectionState.PLAY)
                    if (packet is DisconnectPacket) {
                        assertEquals(Component.text("review kick reason"), packet.message())
                        break
                    }
                }
                // Already queued world packets may follow the kick while Minestom drains its socket.
                assertTrue(socket.getInputStream().readAllBytes().size < 8 * 1024 * 1024)
            }
            service.flush()
            assertEquals(
                Status.Code.FAILED_PRECONDITION,
                assertThrows(StatusRuntimeException::class.java) {
                    stub.preparePlayer(delivery)
                }.status.code,
            )
            val expiring =
                stub.preparePlayer(
                    delivery
                        .toBuilder()
                        .setOperationId("expired")
                        .setOwnerGeneration(2)
                        .build(),
                )
            clock.addAndGet(TimeUnit.SECONDS.toNanos(31))
            service.flush()
            attempt(
                setup
                    .toBuilder()
                    .setOperationId("expired")
                    .setCapability(expiring.capability)
                    .build(),
            ).use {
                assertTrue(it.packet(ConnectionState.LOGIN) is LoginDisconnectPacket)
            }
        } finally {
            service.close()
            channel.shutdownNow().awaitTermination(3, TimeUnit.SECONDS)
            server.shutdownNow().awaitTermination(3, TimeUnit.SECONDS)
            minecraft.stop()
        }
    }
}

internal fun <T> Socket.send(
    id: Int,
    serializer: NetworkBuffer.Type<T>,
    packet: T,
) {
    val body =
        NetworkBuffer.makeArray { buffer ->
            buffer.write(NetworkBuffer.VAR_INT, id)
            buffer.write(serializer, packet)
        }
    getOutputStream().write(
        NetworkBuffer.makeArray { buffer ->
            buffer.write(NetworkBuffer.VAR_INT, body.size)
            buffer.write(NetworkBuffer.RAW_BYTES, body)
        },
    )
}

internal fun Socket.packet(state: ConnectionState): ServerPacket {
    val input = getInputStream()
    var length = 0
    var shift = 0
    while (true) {
        val next = input.read()
        check(next >= 0 && shift < 21) { "Truncated frame" }
        length = length or ((next and 127) shl shift)
        if (next and 128 == 0) break
        shift += 7
    }
    require(length in 1..2_097_151)
    val bytes = input.readNBytes(length)
    check(bytes.size == length)
    val buffer = NetworkBuffer.wrap(bytes, 0, bytes.size, Registries.vanilla())
    return PacketVanilla.SERVER_PACKET_PARSER.parse(state, buffer.read(NetworkBuffer.VAR_INT), buffer)
}
