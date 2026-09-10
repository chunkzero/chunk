package dev.chunkzero.runtime

import chunk.v1.Common.DeploymentRef
import chunk.v1.GameplayGrpc
import chunk.v1.GameplayOuterClass.ConfigurationRequest
import chunk.v1.GameplayOuterClass.ConfigurationResponse
import chunk.v1.GameplayOuterClass.PlayerDelivery
import chunk.v1.GameplayOuterClass.PlayerPreparation
import chunk.v1.GameplayOuterClass.PlayerSetup
import chunk.v1.GameplayOuterClass.PlayerWithdrawal
import chunk.v1.Supervision.DeliveryPhase
import chunk.v1.Supervision.SessionInventory
import io.grpc.Status
import io.grpc.stub.StreamObserver
import net.kyori.adventure.text.Component
import net.minestom.server.MinecraftServer
import net.minestom.server.coordinate.Pos
import net.minestom.server.event.EventNode
import net.minestom.server.event.player.AsyncPlayerConfigurationEvent
import net.minestom.server.event.player.AsyncPlayerPreLoginEvent
import java.util.UUID
import java.util.concurrent.CompletableFuture
import java.util.concurrent.TimeUnit

internal class GameplayService(
    private val deployment: DeploymentRef,
    private val generation: Long,
    private val manager: SessionManager,
    private val now: () -> Long = System::nanoTime,
    private val runtimeId: String = "bridge",
) : GameplayGrpc.GameplayImplBase() {
    private val preparations = mutableMapOf<String, PreparedDelivery>()
    private val owners = DeliveryFence()
    val configurationArtifact =
        ConfigurationResponse
            .newBuilder()
            .setDeployment(deployment)
            .setProcessGeneration(generation)
            .setRuntimeId(runtimeId)
            .setProtocol(MinecraftServer.PROTOCOL_VERSION)
            .build()
    private val events = EventNode.all("gameplay-delivery")
    var endpoint = ""

    init {
        manager.setWithdraw { id ->
            val closing =
                synchronized(
                    preparations,
                ) { preparations.values.filter { it.delivery.session.id == id }.map { it.close() } }
            CompletableFuture.allOf(*closing.toTypedArray())
        }
        events.addListener(AsyncPlayerPreLoginEvent::class.java) { event ->
            try {
                val payload =
                    event
                        .sendPluginRequest(
                            "chunk:delivery",
                            byteArrayOf(),
                        ).get(5, TimeUnit.SECONDS)
                        .payload()
                require(payload != null && payload.size <= 4096)
                val setup = PlayerSetup.parseFrom(payload)
                synchronized(preparations) {
                    val prepared = requireNotNull(preparations[setup.operationId]) { "Unknown operation" }
                    event.gameProfile = prepared.consume(setup, event.gameProfile, event.connection)
                }
            } catch (_: Exception) {
                event.connection.kick(Component.text("Delivery rejected"))
            }
        }
        events.addListener(AsyncPlayerConfigurationEvent::class.java) { event ->
            try {
                val prepared =
                    synchronized(preparations) {
                        requireNotNull(preparations.values.find { it.owns(event.player.playerConnection) })
                    }
                event.spawningInstance = prepared.configure(event.player as ManagedPlayer)
                event.player.respawnPoint = Pos(0.5, 42.0, 0.5)
            } catch (_: Exception) {
                event.player.kick(Component.text("Session unavailable"))
            }
        }
        MinecraftServer.getGlobalEventHandler().addChild(events)
    }

    override fun configuration(
        request: ConfigurationRequest,
        response: StreamObserver<ConfigurationResponse>,
    ) {
        if (request.deployment != deployment) {
            response.onError(Status.PERMISSION_DENIED.withDescription("Deployment mismatch").asRuntimeException())
            return
        }
        response.onNext(configurationArtifact)
        response.onCompleted()
    }

    override fun preparePlayer(
        request: PlayerDelivery,
        response: StreamObserver<PlayerPreparation>,
    ) {
        try {
            val result =
                synchronized(preparations) {
                    validate(request)
                    val previous = preparations[request.operationId]
                    val prepared =
                        if (previous != null) {
                            require(previous.delivery == request) { "Operation reused with different delivery" }
                            previous
                        } else {
                            check(preparations.size < 4096) { "Process preparation history capacity reached" }
                            val session = manager.get(request.session.id, request.sessionGeneration)
                            check(
                                preparations.values.count {
                                    it.delivery.session == request.session && !it.isReleased()
                                } <
                                    session.command.capacity,
                            ) { "Session full" }
                            PreparedDelivery(request, owners, now, session, manager.ticks).also {
                                preparations[request.operationId] =
                                    it
                            }
                        }
                    prepared.result(endpoint)
                }
            response.onNext(result)
            response.onCompleted()
        } catch (_: Exception) {
            response.onError(Status.FAILED_PRECONDITION.withDescription("Delivery rejected").asRuntimeException())
        }
    }

    fun flush() = synchronized(preparations) { preparations.values.forEach { it.checkDeadline() } }

    fun deliveries() = synchronized(preparations) { preparations.values.map { it.inventory() } }

    fun sessions(): List<SessionInventory> =
        manager.inventory().map { session ->
            session
                .toBuilder()
                .setPrepared(
                    deliveries().count {
                        it.delivery.session == session.session &&
                            it.phase == DeliveryPhase.DELIVERY_PHASE_PREPARED
                    },
                ).build()
        }

    override fun withdrawPlayer(
        request: PlayerWithdrawal,
        response: StreamObserver<PlayerWithdrawal>,
    ) {
        val stream = synchronized(preparations) { preparations[request.operationId] }
        if (stream == null || stream.delivery.ownerGeneration != request.ownerGeneration) {
            response.onError(Status.FAILED_PRECONDITION.asRuntimeException())
            return
        }
        stream.close().whenComplete { _, error ->
            if (error !=
                null
            ) {
                response.onError(Status.INTERNAL.withDescription("Withdrawal failed").asRuntimeException())
            } else {
                response.onNext(request)
                response.onCompleted()
            }
        }
    }

    fun close() {
        MinecraftServer.getGlobalEventHandler().removeChild(events)
        synchronized(preparations) { preparations.values.forEach { it.close() } }
    }

    private fun validate(delivery: PlayerDelivery) {
        require(delivery.serializedSize <= 65_536) { "Delivery exceeds size limit" }
        require(
            delivery.deployment == deployment && delivery.processGeneration == generation &&
                delivery.runtimeId == runtimeId,
        ) { "Stale process or deployment" }
        require(delivery.protocol == MinecraftServer.PROTOCOL_VERSION) { "Incompatible destination protocol" }
        require(
            delivery.session.id.isNotBlank() && delivery.operationId.length in 1..128,
        ) { "Unknown session or operation" }
        require(delivery.player.id.isNotBlank() && delivery.ownerGeneration > 0 && delivery.membershipGeneration > 0)
        require(delivery.proxyId.isNotBlank() && delivery.connectionId.isNotBlank())
        require(delivery.identity.username.matches(Regex("[A-Za-z0-9_]{1,16}")))
        require(UUID.fromString(delivery.identity.uuid).toString() == delivery.identity.uuid)
    }
}
