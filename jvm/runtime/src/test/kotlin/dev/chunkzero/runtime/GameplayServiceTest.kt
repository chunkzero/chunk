package dev.chunkzero.runtime

import com.google.protobuf.ByteString
import dev.chunkzero.proto.ConfigurationRequest
import dev.chunkzero.proto.DeploymentRef
import dev.chunkzero.proto.Frame
import dev.chunkzero.proto.GameplayGrpc
import dev.chunkzero.proto.Identity
import dev.chunkzero.proto.PlayerDelivery
import dev.chunkzero.proto.PlayerInput
import dev.chunkzero.proto.PlayerRef
import dev.chunkzero.proto.SessionRef
import io.grpc.Metadata
import io.grpc.Status
import io.grpc.StatusRuntimeException
import io.grpc.netty.shaded.io.grpc.netty.NettyChannelBuilder
import io.grpc.netty.shaded.io.grpc.netty.NettyServerBuilder
import io.grpc.stub.MetadataUtils
import io.grpc.stub.StreamObserver
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
import java.net.InetSocketAddress
import java.util.UUID
import java.util.concurrent.CompletableFuture
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
        val service = GameplayService(deployment, 7, instance)
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
            for (input in listOf(
                PlayerInput.newBuilder().setFrame(Frame.getDefaultInstance()).build(),
                PlayerInput.newBuilder().setDelivery(delivery.toBuilder().setProcessGeneration(6)).build(),
                PlayerInput.newBuilder().setDelivery(delivery.toBuilder().setRegistryDigest(ByteString.EMPTY)).build(),
            )) {
                val completed = CompletableFuture<Status.Code>()
                val stream =
                    GameplayGrpc
                        .newStub(channel)
                        .withInterceptors(interceptor)
                        .withDeadlineAfter(3, TimeUnit.SECONDS)
                        .openPlayer(
                            object : StreamObserver<Frame> {
                                override fun onNext(value: Frame) = Unit

                                override fun onError(error: Throwable) {
                                    completed.complete(Status.fromThrowable(error).code)
                                }

                                override fun onCompleted() {
                                    completed.complete(Status.Code.OK)
                                }
                            },
                        )
                stream.onNext(input)
                assertEquals(Status.Code.INVALID_ARGUMENT, completed.get(3, TimeUnit.SECONDS))
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
}
