package dev.chunkzero.runtime

import chunk.v1.Common.DeploymentRef
import chunk.v1.GameplayGrpc
import chunk.v1.GameplayOuterClass.ConfigurationRequest
import chunk.v1.GameplayOuterClass.ConfigurationResponse
import chunk.v1.GameplayOuterClass.PlayerActivation
import chunk.v1.GameplayOuterClass.PlayerDelivery
import chunk.v1.GameplayOuterClass.PlayerPreparation
import chunk.v1.GameplayOuterClass.PlayerSetup
import chunk.v1.PlayersOuterClass.Frame
import com.google.protobuf.ByteString
import io.grpc.Status
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
import java.net.Socket
import java.security.MessageDigest
import java.security.SecureRandom
import java.util.UUID
import java.util.concurrent.CompletableFuture
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.TimeUnit

internal class GameplayService(
    private val deployment: DeploymentRef,
    private val generation: Long,
    private val instance: InstanceContainer,
    private val now: () -> Long = System::nanoTime,
) : GameplayGrpc.GameplayImplBase() {
    private val preparations = ConcurrentHashMap<String, DeliveryStream>()
    private val tcp =
        PlayerTcp { setup, socket ->
            requireNotNull(preparations[setup.operationId]) { "Unknown operation" }.serve(setup, socket)
        }
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

    override fun preparePlayer(
        request: PlayerDelivery,
        response: StreamObserver<PlayerPreparation>,
    ) {
        reply(response) {
            synchronized(preparations) {
                validate(request)
                val previous = preparations[request.operationId]
                if (previous != null) {
                    require(previous.delivery == request) { "Operation reused with different delivery" }
                    previous.result()
                } else {
                    check(preparations.size < 4096) { "Process preparation history capacity reached" }
                    DeliveryStream(request).also { preparations[request.operationId] = it }.result()
                }
            }
        }
    }

    override fun activatePlayer(
        request: PlayerActivation,
        response: StreamObserver<PlayerPreparation>,
    ) {
        Thread.startVirtualThread {
            reply(response) {
                requireNotNull(preparations[request.operationId]) { "Unknown operation" }.activate(request)
            }
        }
    }

    private fun reply(
        response: StreamObserver<PlayerPreparation>,
        block: () -> PlayerPreparation,
    ) {
        try {
            response.onNext(block())
            response.onCompleted()
        } catch (_: Exception) {
            response.onError(Status.FAILED_PRECONDITION.withDescription("Delivery rejected").asRuntimeException())
        }
    }

    fun flush() = preparations.values.forEach { it.checkDeadline() }

    fun close() {
        tcp.close()
        preparations.values.forEach { it.close() }
    }

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
        require(delivery.serializedSize <= 65_536) { "Delivery exceeds size limit" }
        require(
            delivery.deployment == deployment && delivery.processGeneration == generation,
        ) { "Stale process or deployment" }
        require(
            delivery.protocol == MinecraftServer.PROTOCOL_VERSION &&
                delivery.registryDigest == configuration.registryDigest,
        ) {
            "Incompatible destination registries"
        }
        require(
            delivery.session.id == "bridge" && delivery.operationId.length in 1..128,
        ) { "Unknown session or operation" }
        require(delivery.player.id.isNotBlank() && delivery.ownerGeneration > 0)
        require(delivery.identity.username.matches(Regex("[A-Za-z0-9_]{1,16}")))
        UUID.fromString(delivery.identity.uuid)
    }

    private inner class DeliveryStream(
        val delivery: PlayerDelivery,
    ) {
        private val capability = ByteArray(32).also { SecureRandom().nextBytes(it) }
        private val openedAt = now()
        private var socket: Socket? = null
        private var consumed = false
        private var activation: PlayerActivation? = null
        private val initialized = CompletableFuture<Unit>()
        private var connection: FrameConnection? = null
        private var claimed = false
        private var closed = false

        @Volatile private var writingAt = 0L

        @Synchronized
        fun result(): PlayerPreparation {
            check(!closed) { "Delivery closed" }
            return PlayerPreparation
                .newBuilder()
                .setOperationId(delivery.operationId)
                .setEndpoint(tcp.endpoint)
                .setCapability(ByteString.copyFrom(capability))
                .build()
        }

        fun serve(
            setup: PlayerSetup,
            accepted: Socket,
        ) {
            synchronized(this) {
                check(!closed && !consumed && now() - openedAt < TimeUnit.SECONDS.toNanos(30))
                require(MessageDigest.isEqual(capability, setup.capability.toByteArray()))
                consumed = true
                socket = accepted
                accepted.getOutputStream().write(0)
                accepted.soTimeout = 0
            }
            try {
                while (true) {
                    val frame = PlayerTcp.readFrame(accepted.getInputStream(), 65_536)
                    synchronized(this) {
                        check(!closed && initialized.isDone && !initialized.isCompletedExceptionally)
                        requireNotNull(connection).receive(frame)
                    }
                }
            } finally {
                close()
            }
        }

        fun activate(request: PlayerActivation): PlayerPreparation {
            val fresh: FrameConnection?
            synchronized(this) {
                check(!closed && consumed && (activation != null || now() - openedAt < TimeUnit.SECONDS.toNanos(30)))
                require(request.clientInformation.size() <= 8192)
                val previous = activation
                if (previous != null) {
                    require(previous == request) { "Activation arguments changed" }
                    fresh = null
                } else {
                    check(requireNotNull(socket).getInputStream().available() == 0) { "Premature play bytes" }
                    owners.claim(delivery.identity.uuid, delivery.ownerGeneration)
                    claimed = true
                    activation = request
                    fresh = FrameConnection()
                    connection = fresh
                }
            }
            if (fresh != null) {
                try {
                    initialize(fresh, delivery, request.clientInformation.toByteArray())
                    initialized.complete(Unit)
                    Thread.startVirtualThread { writePackets(fresh) }
                } catch (error: Exception) {
                    initialized.completeExceptionally(error)
                    close()
                    throw error
                }
            }
            initialized.get(5, TimeUnit.SECONDS)
            return result()
        }

        private fun writePackets(current: FrameConnection) {
            try {
                val output = requireNotNull(socket).getOutputStream()
                while (current.isOnline) {
                    val bytes = current.poll()
                    if (bytes == null) {
                        Thread.sleep(5)
                    } else {
                        writingAt = now()
                        PlayerTcp.writeFrame(output, bytes)
                        writingAt = 0
                    }
                }
            } catch (_: Exception) {
                // Closing the socket also interrupts a blocked reader or writer.
            } finally {
                close()
            }
        }

        private fun initialize(
            fresh: FrameConnection,
            delivery: PlayerDelivery,
            settingsBytes: ByteArray,
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
        fun checkDeadline() {
            val timestamp = now()
            if ((activation == null && timestamp - openedAt > TimeUnit.SECONDS.toNanos(30)) ||
                (writingAt != 0L && timestamp - writingAt > TimeUnit.SECONDS.toNanos(5)) ||
                connection?.isOnline == false
            ) {
                close()
            }
        }

        @Synchronized
        fun close() {
            if (closed) return
            closed = true
            socket?.close()
            connection?.disconnect()
            if (claimed) owners.release(delivery.identity.uuid, delivery.ownerGeneration)
            initialized.completeExceptionally(IllegalStateException("Delivery closed"))
        }
    }
}
