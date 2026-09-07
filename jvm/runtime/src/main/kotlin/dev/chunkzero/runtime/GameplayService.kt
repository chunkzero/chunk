package dev.chunkzero.runtime

import chunk.v1.Common.DeploymentRef
import chunk.v1.Common.SessionRef
import chunk.v1.GameplayGrpc
import chunk.v1.GameplayOuterClass.ConfigurationRequest
import chunk.v1.GameplayOuterClass.ConfigurationResponse
import chunk.v1.GameplayOuterClass.PlayerDelivery
import chunk.v1.GameplayOuterClass.PlayerPreparation
import chunk.v1.GameplayOuterClass.PlayerSetup
import chunk.v1.Supervision.DeliveryPhase
import chunk.v1.Supervision.SessionInventory
import chunk.v1.Supervision.SessionPhase
import io.grpc.Status
import io.grpc.stub.StreamObserver
import net.kyori.adventure.text.Component
import net.minestom.server.MinecraftServer
import net.minestom.server.coordinate.Pos
import net.minestom.server.event.EventNode
import net.minestom.server.event.player.AsyncPlayerConfigurationEvent
import net.minestom.server.event.player.AsyncPlayerPreLoginEvent
import net.minestom.server.instance.InstanceContainer
import java.util.UUID
import java.util.concurrent.TimeUnit

internal class GameplayService(
    private val deployment: DeploymentRef,
    private val generation: Long,
    private val instance: InstanceContainer,
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
            event.spawningInstance = instance
            event.player.respawnPoint = Pos(0.5, 42.0, 0.5)
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
                            PreparedDelivery(request, owners, now).also { preparations[request.operationId] = it }
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

    fun sessions(): List<SessionInventory> {
        val deliveries = deliveries()
        return listOf(
            SessionInventory
                .newBuilder()
                .setSession(SessionRef.newBuilder().setId("bridge"))
                .setGeneration(1)
                .setSessionType("bridge")
                .setPhase(SessionPhase.SESSION_PHASE_READY)
                .setCapacity(128)
                .setPrepared(deliveries.count { it.phase == DeliveryPhase.DELIVERY_PHASE_PREPARED })
                .setAttached(deliveries.count { it.phase == DeliveryPhase.DELIVERY_PHASE_ATTACHED })
                .build(),
        )
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
            delivery.session.id == "bridge" && delivery.operationId.length in 1..128,
        ) { "Unknown session or operation" }
        require(delivery.player.id.isNotBlank() && delivery.ownerGeneration > 0)
        require(delivery.identity.username.matches(Regex("[A-Za-z0-9_]{1,16}")))
        UUID.fromString(delivery.identity.uuid)
    }
}
