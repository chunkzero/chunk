package dev.chunkzero.runtime

import chunk.v1.Common.DeploymentRef
import chunk.v1.Common.Identity
import chunk.v1.Common.PlayerRef
import chunk.v1.Common.SessionRef
import chunk.v1.GameplayGrpc
import chunk.v1.GameplayOuterClass.ConfigurationRequest
import chunk.v1.GameplayOuterClass.PlayerDelivery
import chunk.v1.GameplayOuterClass.PlayerSetup
import com.google.protobuf.ByteString
import io.grpc.Metadata
import io.grpc.Status
import io.grpc.StatusRuntimeException
import io.grpc.netty.shaded.io.grpc.netty.NettyChannelBuilder
import io.grpc.netty.shaded.io.grpc.netty.NettyServerBuilder
import io.grpc.stub.MetadataUtils
import net.minestom.server.MinecraftServer
import net.minestom.server.network.ConnectionState
import net.minestom.server.network.NetworkBuffer
import net.minestom.server.network.packet.PacketVanilla
import net.minestom.server.network.packet.client.common.ClientSettingsPacket
import net.minestom.server.network.packet.server.common.KeepAlivePacket
import net.minestom.server.network.player.ClientSettings
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.net.InetSocketAddress
import java.net.Socket
import java.util.UUID
import java.util.concurrent.TimeUnit

class GameplayServiceTest {
    @Test
    fun `authenticated configuration and delivery reject incompatible or replayed input`() {
        MinecraftServer.init()
        val deployment =
            DeploymentRef
                .newBuilder()
                .setEnvironment("local")
                .setDeployment("build-a")
                .build()
        val instance = MinecraftServer.getInstanceManager().createInstanceContainer()
        val clock =
            java.util.concurrent.atomic
                .AtomicLong(System.nanoTime())
        val service = GameplayService(deployment, 7, instance, clock::get)
        val server =
            NettyServerBuilder
                .forAddress(InetSocketAddress("127.0.0.1", 0))
                .intercept(ProcessAuthentication("test-token"))
                .addService(service)
                .build()
                .start()
        val channel = NettyChannelBuilder.forAddress("127.0.0.1", server.port).usePlaintext().build()
        try {
            val request = ConfigurationRequest.newBuilder().setDeployment(deployment).build()
            val unauthenticated = GameplayGrpc.newBlockingStub(channel).withDeadlineAfter(3, TimeUnit.SECONDS)
            assertEquals(
                Status.Code.UNAUTHENTICATED,
                assertThrows(StatusRuntimeException::class.java) {
                    unauthenticated.configuration(request)
                }.status.code,
            )
            val headers = Metadata()
            headers.put(Metadata.Key.of("authorization", Metadata.ASCII_STRING_MARSHALLER), "Bearer test-token")
            val interceptor = MetadataUtils.newAttachHeadersInterceptor(headers)
            val stub = unauthenticated.withInterceptors(interceptor)
            val configuration = stub.configuration(request)
            assertEquals(775, configuration.protocol)
            assertEquals(7, configuration.processGeneration)
            assertEquals(32, configuration.registryDigest.size())
            assertTrue(configuration.packetsCount > 10)
            configuration.packetsList.forEach {
                val bytes = it.packet.toByteArray()
                val buffer = NetworkBuffer.wrap(bytes, 0, bytes.size, MinecraftServer.process())
                PacketVanilla.SERVER_PACKET_PARSER.parse(
                    ConnectionState.CONFIGURATION,
                    buffer.read(NetworkBuffer.VAR_INT),
                    buffer,
                )
                assertEquals(0, buffer.readableBytes())
            }
            assertEquals(
                Status.Code.PERMISSION_DENIED,
                assertThrows(StatusRuntimeException::class.java) {
                    stub.configuration(
                        request.toBuilder().setDeployment(deployment.toBuilder().setDeployment("other")).build(),
                    )
                }.status.code,
            )
            val settings =
                NetworkBuffer.makeArray(
                    ClientSettingsPacket.SERIALIZER,
                    ClientSettingsPacket(ClientSettings.DEFAULT),
                )
            val delivery =
                PlayerDelivery
                    .newBuilder()
                    .setDeployment(deployment)
                    .setProcessGeneration(7)
                    .setOwnerGeneration(1)
                    .setOperationId("delivery-1")
                    .setSession(SessionRef.newBuilder().setId("bridge"))
                    .setPlayer(PlayerRef.newBuilder().setId("player"))
                    .setIdentity(Identity.newBuilder().setUuid(UUID.randomUUID().toString()).setUsername("player"))
                    .setProtocol(
                        775,
                    ).setRegistryDigest(
                        configuration.registryDigest,
                    ).setClientInformation(ByteString.copyFrom(settings))
                    .build()
            for (invalid in listOf(
                delivery.toBuilder().setProcessGeneration(6).build(),
                delivery.toBuilder().setRegistryDigest(ByteString.EMPTY).build(),
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
            val address = InetSocketAddress("127.0.0.1", prepared.endpoint.substringAfter(':').toInt())
            val setup =
                PlayerSetup
                    .newBuilder()
                    .setOperationId(delivery.operationId)
                    .setCapability(prepared.capability)
                    .build()
            Socket().use { bad ->
                bad.connect(address)
                bad.soTimeout = 3000
                PlayerTcp.writeFrame(
                    bad.getOutputStream(),
                    setup
                        .toBuilder()
                        .setCapability(ByteString.EMPTY)
                        .build()
                        .toByteArray(),
                )
                assertEquals(-1, bad.getInputStream().read())
            }
            Socket().use { socket ->
                socket.connect(address)
                socket.soTimeout = 3000
                val wire = ByteArrayOutputStream()
                PlayerTcp.writeFrame(wire, setup.toByteArray())
                wire.toByteArray().forEach { socket.getOutputStream().write(it.toInt()) }
                assertEquals(0, socket.getInputStream().read())
                assertTrue(MinecraftServer.getConnectionManager().onlinePlayers.isEmpty())
                Socket().use { duplicate ->
                    duplicate.connect(address)
                    duplicate.soTimeout = 3000
                    PlayerTcp.writeFrame(duplicate.getOutputStream(), setup.toByteArray())
                    assertEquals(-1, duplicate.getInputStream().read())
                }
                // Gameplay before activation tears down the preparation without creating a player.
                PlayerTcp.writeFrame(socket.getOutputStream(), byteArrayOf(0))
                assertEquals(-1, socket.getInputStream().read())
                assertTrue(MinecraftServer.getConnectionManager().onlinePlayers.isEmpty())
            }
            val expiring = stub.preparePlayer(delivery.toBuilder().setOperationId("expired").build())
            clock.addAndGet(TimeUnit.SECONDS.toNanos(31))
            service.flush()
            Socket().use { expired ->
                expired.connect(address)
                expired.soTimeout = 3000
                PlayerTcp.writeFrame(
                    expired.getOutputStream(),
                    setup
                        .toBuilder()
                        .setOperationId("expired")
                        .setCapability(expiring.capability)
                        .build()
                        .toByteArray(),
                )
                assertEquals(-1, expired.getInputStream().read())
            }
            val connection = FrameConnection()
            repeat(257) { connection.sendPacket(KeepAlivePacket(1)) }
            assertFalse(connection.isOnline)
            assertEquals(null, connection.poll())
        } finally {
            service.close()
            channel.shutdownNow().awaitTermination(3, TimeUnit.SECONDS)
            server.shutdownNow().awaitTermination(3, TimeUnit.SECONDS)
            MinecraftServer.process().stop()
        }
    }

    @Test
    fun `TCP framing retains coalesced packets and rejects truncation and oversized lengths`() {
        val output = ByteArrayOutputStream()
        PlayerTcp.writeFrame(output, byteArrayOf(1, 2, 3))
        PlayerTcp.writeFrame(output, ByteArray(1024) { 7 })
        val input = ByteArrayInputStream(output.toByteArray())
        assertTrue(PlayerTcp.readFrame(input, 1024).contentEquals(byteArrayOf(1, 2, 3)))
        assertEquals(1024, PlayerTcp.readFrame(input, 1024).size)
        for (bytes in listOf(byteArrayOf(0), byteArrayOf(5, 1), byteArrayOf(-128, -128, -128), byteArrayOf(127))) {
            assertThrows(Exception::class.java) { PlayerTcp.readFrame(ByteArrayInputStream(bytes), 16) }
        }
    }
}
