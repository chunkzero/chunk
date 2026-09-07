package dev.chunkzero.runtime

import com.google.protobuf.ByteString
import dev.chunkzero.proto.ConfigurationRequest
import dev.chunkzero.proto.ConfigurationResponse
import dev.chunkzero.proto.DeploymentRef
import dev.chunkzero.proto.Frame
import dev.chunkzero.proto.GameplayGrpc
import dev.chunkzero.proto.PlayerDelivery
import dev.chunkzero.proto.PlayerInput
import io.grpc.Status
import io.grpc.stub.ServerCallStreamObserver
import io.grpc.stub.StreamObserver
import net.minestom.server.MinecraftServer
import net.minestom.server.coordinate.Pos
import net.minestom.server.instance.InstanceContainer
import net.minestom.server.network.ConnectionState
import net.minestom.server.network.NetworkBuffer
import net.minestom.server.network.packet.client.common.ClientSettingsPacket
import net.minestom.server.network.packet.server.configuration.UpdateEnabledFeaturesPacket
import net.minestom.server.network.player.GameProfile
import net.minestom.server.registry.Registries
import java.security.MessageDigest
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap

internal class GameplayService(
    private val deployment: DeploymentRef,
    private val generation: Long,
    private val instance: InstanceContainer,
) : GameplayGrpc.GameplayImplBase() {
    private val connections = ConcurrentHashMap.newKeySet<DeliveryStream>()
    private val owners = DeliveryFence()
    private val configuration = configurationSnapshot()

    override fun configuration(
        request: ConfigurationRequest,
        response: StreamObserver<ConfigurationResponse>,
    ) {
        if (request.deployment != deployment) {
            response.onError(Status.PERMISSION_DENIED.withDescription("Deployment mismatch").asRuntimeException())
            return
        }
        response.onNext(configuration)
        response.onCompleted()
    }

    override fun openPlayer(responseObserver: StreamObserver<Frame>): StreamObserver<PlayerInput> {
        val output = responseObserver as ServerCallStreamObserver<Frame>
        val stream = DeliveryStream(output)
        output.disableAutoRequest()
        output.setOnCancelHandler { stream.close() }
        output.setOnReadyHandler { stream.flush() }
        output.request(1)
        connections.add(stream)
        return stream
    }

    fun flush() = connections.forEach { it.flush() }

    fun close() = connections.toList().forEach { it.close() }

    private fun configurationSnapshot(): ConfigurationResponse {
        val packets =
            buildList {
                add(UpdateEnabledFeaturesPacket(listOf("minecraft:vanilla")))
                addAll(Registries.registryDataPackets(MinecraftServer.process(), false))
                add(Registries.tagsPacket(MinecraftServer.process()))
            }.map { FrameConnection.encode(ConnectionState.CONFIGURATION, it) }
        val digest = MessageDigest.getInstance("SHA-256")
        packets.forEach { digest.update(it) }
        return ConfigurationResponse
            .newBuilder()
            .setDeployment(deployment)
            .setProcessGeneration(generation)
            .setProtocol(MinecraftServer.PROTOCOL_VERSION)
            .setRegistryDigest(ByteString.copyFrom(digest.digest()))
            .addAllPackets(packets.map { Frame.newBuilder().setPacket(ByteString.copyFrom(it)).build() })
            .build()
    }

    private fun validate(delivery: PlayerDelivery) {
        require(
            delivery.deployment == deployment && delivery.processGeneration == generation,
        ) { "Stale process or deployment" }
        require(
            delivery.protocol == MinecraftServer.PROTOCOL_VERSION &&
                delivery.registryDigest == configuration.registryDigest,
        ) {
            "Incompatible destination registries"
        }
        require(delivery.session.id == "bridge" && delivery.operationId.isNotBlank()) { "Unknown session or operation" }
        require(delivery.player.id.isNotBlank() && delivery.ownerGeneration > 0)
        require(delivery.identity.username.matches(Regex("[A-Za-z0-9_]{1,16}")))
        UUID.fromString(delivery.identity.uuid)
    }

    private inner class DeliveryStream(
        private val output: ServerCallStreamObserver<Frame>,
    ) : StreamObserver<PlayerInput> {
        private var connection: FrameConnection? = null
        private var delivery: PlayerDelivery? = null
        private var closed = false
        private val openedAt = System.nanoTime()

        @Synchronized
        override fun onNext(value: PlayerInput) {
            if (closed) return
            try {
                val current = connection
                if (current == null) {
                    require(value.hasDelivery()) { "First message must authenticate a delivery" }
                    validate(value.delivery)
                    owners.claim(value.delivery.player.id, value.delivery.ownerGeneration)
                    delivery = value.delivery
                    val fresh = FrameConnection()
                    connection = fresh
                    // Player creation may invoke a provider that needs a virtual thread.
                    Thread.startVirtualThread {
                        try {
                            initialize(fresh, value.delivery)
                            synchronized(this) { if (!closed) output.request(1) }
                        } catch (error: Exception) {
                            fail(error)
                        }
                    }
                } else {
                    require(value.hasFrame()) { "Delivery cannot be replayed on a player stream" }
                    current.receive(value.frame.packet.toByteArray())
                    output.request(1)
                }
            } catch (error: Exception) {
                fail(error)
            }
        }

        private fun initialize(
            fresh: FrameConnection,
            delivery: PlayerDelivery,
        ) {
            val identity = delivery.identity
            val profile =
                GameProfile(
                    UUID.fromString(identity.uuid),
                    identity.username,
                    identity.propertiesList.map {
                        GameProfile.Property(
                            it.name,
                            it.value,
                            if (it.hasSignature()) it.signature else null,
                        )
                    },
                )
            val player = MinecraftServer.getConnectionManager().createPlayer(fresh, profile)
            val settingsBytes = delivery.clientInformation.toByteArray()
            val buffer = NetworkBuffer.wrap(settingsBytes, 0, settingsBytes.size, MinecraftServer.process())
            val settings = ClientSettingsPacket.SERIALIZER.read(buffer)
            require(buffer.readableBytes() == 0L)
            player.refreshSettings(settings.settings())
            player.respawnPoint = Pos(0.5, 42.0, 0.5)
            player.setPendingOptions(instance, false)
            synchronized(this) {
                if (closed) {
                    fresh.disconnect()
                } else {
                    MinecraftServer.getConnectionManager().transitionConfigToPlay(
                        player,
                    )
                }
            }
        }

        @Synchronized
        fun flush() {
            if (closed) return
            val current = connection
            if (current == null) {
                if (System.nanoTime() - openedAt >
                    java.util.concurrent.TimeUnit.SECONDS
                        .toNanos(10)
                ) {
                    close(Status.DEADLINE_EXCEEDED.withDescription("Delivery deadline expired"))
                }
                return
            }
            if (!current.isOnline) {
                close()
                return
            }
            while (output.isReady && !output.isCancelled) {
                val bytes = current.poll() ?: break
                output.onNext(Frame.newBuilder().setPacket(ByteString.copyFrom(bytes)).build())
            }
        }

        @Synchronized
        private fun fail(error: Exception) {
            close(Status.INVALID_ARGUMENT.withDescription(error.message))
        }

        @Synchronized
        fun close(error: Status? = null) {
            if (closed) return
            closed = true
            connection?.disconnect()
            delivery?.let { owners.release(it.player.id, it.ownerGeneration) }
            connections.remove(this)
            if (!output.isCancelled) {
                if (error == null) output.onCompleted() else output.onError(error.asRuntimeException())
            }
        }

        override fun onError(error: Throwable) = close()

        override fun onCompleted() = close()
    }
}
