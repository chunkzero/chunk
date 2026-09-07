package dev.chunkzero.runtime

import chunk.v1.Common.Identity
import chunk.v1.GameplayOuterClass.PlayerDelivery
import chunk.v1.GameplayOuterClass.PlayerPreparation
import chunk.v1.GameplayOuterClass.PlayerSetup
import com.google.protobuf.ByteString
import net.minestom.server.network.player.GameProfile
import net.minestom.server.network.player.PlayerConnection
import java.security.MessageDigest
import java.security.SecureRandom
import java.util.UUID
import java.util.concurrent.TimeUnit

/** Single-use admission record; terminal history retains no live player references. */
internal class PreparedDelivery(
    val delivery: PlayerDelivery,
    private val owners: DeliveryFence,
    private val now: () -> Long,
) {
    private val capability = ByteArray(32).also { SecureRandom().nextBytes(it) }
    private val openedAt = now()
    private var consumed = false
    private var closed = false
    private var connection: PlayerConnection? = null

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

    fun consume(
        setup: PlayerSetup,
        presented: GameProfile,
        accepted: PlayerConnection,
    ): GameProfile {
        checkDeadline()
        check(!closed && !consumed)
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

    fun checkDeadline() {
        if ((!consumed && now() - openedAt >= TimeUnit.SECONDS.toNanos(30)) || connection?.isOnline == false) close()
    }

    fun close() {
        if (closed) return
        closed = true
        val current = connection
        connection = null
        current?.disconnect()
        if (consumed) owners.release(delivery.identity.uuid, delivery.ownerGeneration)
    }
}

private fun Identity.profile() =
    GameProfile(
        UUID.fromString(uuid),
        username,
        propertiesList.map { GameProfile.Property(it.name, it.value, if (it.hasSignature()) it.signature else null) },
    )
