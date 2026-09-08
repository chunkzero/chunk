package dev.chunkzero.runtime

import net.minestom.server.entity.Player
import net.minestom.server.network.player.GameProfile
import net.minestom.server.network.player.PlayerConnection
import java.util.concurrent.CompletableFuture

internal class ManagedPlayer(
    connection: PlayerConnection,
    profile: GameProfile,
) : Player(connection, profile) {
    var initialization: CompletableFuture<Void>? = null
        private set

    override fun UNSAFE_init(): CompletableFuture<Void> {
        val completion = CompletableFuture<Void>()
        initialization = completion
        try {
            super.UNSAFE_init().whenComplete { _, error ->
                if (error == null) completion.complete(null) else completion.completeExceptionally(error)
            }
        } catch (error: Exception) {
            completion.completeExceptionally(error)
        }
        return completion
    }
}
