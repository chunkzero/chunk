package com.chunkzero.chunk.runtime.minestom.internal

import chunk.sync.v1.CoreOuterClass.Position
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Test

class DeliveryFenceTest {
    private fun at(
        revision: Long,
        epoch: Long = 1,
    ) = Position
        .newBuilder()
        .setEpoch(epoch)
        .setRevision(revision)
        .build()

    @Test
    fun `active delivery cannot be duplicated and stale releases cannot withdraw a replacement`() {
        val fence = DeliveryFence()
        fence.claim("player", at(1))
        assertThrows(IllegalArgumentException::class.java) { fence.claim("player", at(1)) }
        assertThrows(IllegalArgumentException::class.java) { fence.claim("player", at(2)) }
        fence.release("player", at(1))
        assertThrows(IllegalArgumentException::class.java) { fence.claim("player", at(1)) }
        fence.claim("player", at(2))
        fence.release("player", at(1))
        assertThrows(IllegalArgumentException::class.java) { fence.claim("player", at(3)) }
        fence.release("player", at(2))
        // A restore's new epoch orders after every revision of the previous one.
        fence.claim("player", at(1, epoch = 2))
        fence.claim("other", at(1))
        assertThrows(IllegalArgumentException::class.java) { fence.claim("unset", Position.getDefaultInstance()) }
    }
}
