package dev.chunkzero.runtime

/** Retains fencing after disconnect; a player stream is never replayed. */
internal class DeliveryFence {
    private data class Owner(
        val generation: Long,
        var active: Boolean,
    )

    private val owners = mutableMapOf<String, Owner>()

    @Synchronized
    fun claim(
        player: String,
        generation: Long,
    ) {
        require(player.isNotBlank() && generation > 0)
        val previous = owners[player]
        require(previous == null || generation > previous.generation) { "Stale delivery generation" }
        require(previous?.active != true) { "Player already delivered" }
        check(previous != null || owners.size < 65_536) { "Process delivery history capacity reached" }
        owners[player] = Owner(generation, active = true)
    }

    @Synchronized
    fun release(
        player: String,
        generation: Long,
    ) {
        owners[player]?.takeIf { it.generation == generation }?.active = false
    }
}
