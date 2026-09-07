package dev.chunkzero.runtime

import chunk.v1.Common.Identity
import chunk.v1.GameplayOuterClass.PlayerDelivery
import chunk.v1.GameplayOuterClass.PlayerPreparation
import chunk.v1.GameplayOuterClass.PlayerSetup
import chunk.v1.Supervision.DeliveryInventory
import chunk.v1.Supervision.DeliveryPhase
import chunk.v1.Supervision.SessionPhase
import com.google.protobuf.ByteString
import net.minestom.server.MinecraftServer
import net.minestom.server.instance.InstanceContainer
import net.minestom.server.network.ConnectionState
import net.minestom.server.network.player.GameProfile
import net.minestom.server.network.player.PlayerConnection
import java.security.MessageDigest
import java.security.SecureRandom
import java.util.UUID
import java.util.concurrent.CompletableFuture
import java.util.concurrent.TimeUnit

/** Single-use admission record; terminal history retains no live player references. */
internal class PreparedDelivery(
    val delivery: PlayerDelivery,
    private val owners: DeliveryFence,
    private val now: () -> Long,
    private val session: SessionManager.ManagedSession,
    private val ticks: TickExecutor,
) {
    private val capability = ByteArray(32).also { SecureRandom().nextBytes(it) }
    private val openedAt = now()
    private var consumed = false
    private var closed = false
    private var connection: PlayerConnection? = null
    private var player: ManagedPlayer? = null
    private var joining = CompletableFuture.completedFuture(Unit)
    private val removed = CompletableFuture<Unit>()
    private var arrived = false
    private var joinStarted = false

    @Synchronized fun owns(current: PlayerConnection) = !closed && connection === current

    @Synchronized fun isReleased() = removed.isDone && !removed.isCompletedExceptionally

    @Synchronized
    fun configure(current: ManagedPlayer): InstanceContainer {
        check(!closed && connection === current.playerConnection)
        check(session.phase == SessionPhase.SESSION_PHASE_READY)
        current.binding = delivery
        player = current
        return session.scope.instances.first()
    }

    @Synchronized
    fun inventory(): DeliveryInventory {
        checkDeadline()
        return DeliveryInventory
            .newBuilder()
            .setDelivery(delivery.toBuilder().clearIdentity())
            .setPhase(
                when {
                    closed && !isReleased() -> DeliveryPhase.DELIVERY_PHASE_WITHDRAWING
                    closed -> DeliveryPhase.DELIVERY_PHASE_CLOSED
                    arrived -> DeliveryPhase.DELIVERY_PHASE_ARRIVED
                    connection?.clientState == ConnectionState.PLAY -> DeliveryPhase.DELIVERY_PHASE_ATTACHED
                    else -> DeliveryPhase.DELIVERY_PHASE_PREPARED
                },
            ).build()
    }

    @Synchronized
    fun result(endpoint: String): PlayerPreparation {
        checkDeadline()
        check(!closed)
        return PlayerPreparation
            .newBuilder()
            .setOperationId(delivery.operationId)
            .setEndpoint(endpoint)
            .setCapability(ByteString.copyFrom(capability))
            .build()
    }

    @Synchronized
    fun consume(
        setup: PlayerSetup,
        presented: GameProfile,
        accepted: PlayerConnection,
    ): GameProfile {
        checkDeadline()
        check(!closed && !consumed && session.phase == SessionPhase.SESSION_PHASE_READY)
        require(
            setup.operationId == delivery.operationId &&
                MessageDigest.isEqual(capability, setup.capability.toByteArray()),
        )
        val profile = delivery.identity.profile()
        require(presented.uuid() == profile.uuid() && presented.name() == profile.name())
        owners.claim(delivery.identity.uuid, delivery.ownerGeneration)
        consumed = true
        connection = accepted
        return profile
    }

    @Synchronized
    fun checkDeadline() {
        player?.let {
            val spawned =
                it.initialization?.let { completion -> completion.isDone && !completion.isCompletedExceptionally } ==
                    true
            if (spawned && !closed && !joinStarted) {
                joinStarted = true
                joining = session.join(it)
                joining.whenComplete { _, error -> if (error != null) close() }
            }
            if (spawned && joinStarted && joining.isDone && !joining.isCompletedExceptionally &&
                it.lastSentTeleportId > 0 && it.lastReceivedTeleportId == it.lastSentTeleportId
            ) {
                arrived = true
            }
        }
        if ((!consumed && now() - openedAt >= TimeUnit.SECONDS.toNanos(30)) || connection?.isOnline == false) close()
    }

    @Synchronized
    fun close(): CompletableFuture<Unit> {
        if (closed) return removed
        closed = true
        val current = connection
        current?.disconnect()
        joining
            .handle { _, _ -> Unit }
            .thenCompose {
                ticks.submit { player?.initialization ?: CompletableFuture.completedFuture(null) }.thenCompose {
                    it.handle {
                        _,
                        _,
                        ->
                        Unit
                    }
                }
            }.thenCompose {
                ticks.submit {
                    player?.let {
                        MinecraftServer.getConnectionManager().removePlayer(requireNotNull(current))
                        if (it.instance != null && !it.isRemoved) it.remove()
                    }
                    player
                }
            }.thenCompose { currentPlayer ->
                if (currentPlayer == null) CompletableFuture.completedFuture(Unit) else session.leave(currentPlayer)
            }.whenComplete { _, error ->
                synchronized(this) {
                    connection = null
                    player = null
                    if (error == null) {
                        if (consumed) owners.release(delivery.identity.uuid, delivery.ownerGeneration)
                        removed.complete(Unit)
                    } else {
                        removed.completeExceptionally(error)
                    }
                }
            }
        return removed
    }
}

private fun Identity.profile() =
    GameProfile(
        UUID.fromString(uuid),
        username,
        propertiesList.map { GameProfile.Property(it.name, it.value, if (it.hasSignature()) it.signature else null) },
    )
