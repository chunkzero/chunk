package dev.chunkzero.runtime.delivery

import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Test

class DeliveryFenceTest {
    @Test
    fun `active delivery cannot be duplicated and stale releases cannot withdraw a replacement`() {
        val fence = DeliveryFence()
        fence.claim("player", 1)
        assertThrows(IllegalArgumentException::class.java) { fence.claim("player", 1) }
        assertThrows(IllegalArgumentException::class.java) { fence.claim("player", 2) }
        fence.release("player", 1)
        assertThrows(IllegalArgumentException::class.java) { fence.claim("player", 1) }
        fence.claim("player", 2)
        fence.release("player", 1)
        assertThrows(IllegalArgumentException::class.java) { fence.claim("player", 3) }
        fence.release("player", 2)
        fence.claim("player", 3)
        fence.claim("other", 1)
    }
}
