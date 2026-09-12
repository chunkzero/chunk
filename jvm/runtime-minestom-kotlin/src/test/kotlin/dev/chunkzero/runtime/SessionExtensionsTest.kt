package dev.chunkzero.runtime

import net.minestom.server.MinecraftServer
import net.minestom.server.entity.Player
import net.minestom.server.network.packet.server.SendablePacket
import net.minestom.server.network.player.GameProfile
import net.minestom.server.network.player.PlayerConnection
import org.junit.jupiter.api.AfterEach
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Assertions.assertSame
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.BeforeEach
import org.junit.jupiter.api.Test
import java.net.InetSocketAddress
import java.util.UUID
import java.util.concurrent.CompletableFuture
import kotlin.time.Duration.Companion.seconds

class SessionExtensionsTest {
    private lateinit var scope: SessionScope

    @BeforeEach
    fun start() {
        MinecraftServer.init()
        val ticks = TickExecutor()
        ticks.flush()
        scope = SessionScope("extensions", 1, ticks, { CompletableFuture.completedFuture(null) }, null)
    }

    @AfterEach
    fun stop() {
        try {
            scope.players.clear()
            scope.dispose()
        } finally {
            MinecraftServer.process().stop()
        }
    }

    @Test
    fun `resources and tasks share session ownership`() {
        val resource = scope.resource<Counter> { Counter() }
        assertSame(resource, scope.resource<Counter> { error("Resource created twice") })
        var runs = 0
        scope.repeatEvery(1.seconds) { runs++ }
        val scheduler = MinecraftServer.getSchedulerManager()
        val task = scheduler.buildTask { runs++ }.schedule()
        assertSame(task, scope.own(task))

        scope.dispose()
        scheduler.process()
        assertEquals(1, resource.closed)
        assertEquals(0, runs, "Disposal must cancel a task that has not started")
        assertFalse(task.isAlive)
    }

    @Test
    fun `player tasks cancel on departure and rejected ownership`() {
        val admitted = player("admitted")
        val outsider = player("outsider")
        scope.players.add(admitted)
        val scheduler = MinecraftServer.getSchedulerManager()
        var playerRuns = 0
        var sessionRuns = 0
        val owned = scheduler.buildTask { playerRuns++ }.schedule()
        val rejected = scheduler.buildTask { playerRuns++ }.schedule()
        assertSame(owned, scope.own(admitted, owned))
        assertThrows(IllegalStateException::class.java) { scope.own(outsider, rejected) }
        scope.own(scheduler.buildTask { sessionRuns++ }.schedule())

        scope.releasePlayer(admitted)
        scheduler.process()
        assertFalse(owned.isAlive)
        assertFalse(rejected.isAlive)
        assertEquals(0, playerRuns)
        assertEquals(1, sessionRuns)
    }

    private class Counter : AutoCloseable {
        var closed = 0

        override fun close() {
            closed++
        }
    }

    private fun player(name: String) =
        Player(
            object : PlayerConnection() {
                override fun sendPacket(packet: SendablePacket) {}

                override fun getRemoteAddress() = InetSocketAddress("127.0.0.1", 0)
            },
            GameProfile(UUID.randomUUID(), name),
        )
}
