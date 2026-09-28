package dev.chunkzero.runtime

import chunk.sync.v1.Jvm.JvmSession
import chunk.sync.v1.Jvm.JvmSessionPhase
import dev.chunkzero.runtime.bootstrap.FlatSession
import dev.chunkzero.runtime.minestom.event.SessionCreateEvent
import net.minestom.server.ServerProcess
import net.minestom.server.entity.Player
import net.minestom.server.network.packet.server.SendablePacket
import net.minestom.server.network.player.GameProfile
import net.minestom.server.network.player.PlayerConnection
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Assertions.assertSame
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import java.net.InetSocketAddress
import java.util.UUID
import java.util.concurrent.CompletableFuture
import java.util.function.Supplier

class SessionManagerTest {
    @Test
    fun `independent processes keep session events instances and shutdown isolated`() {
        ServerProcess.create().use { first ->
            ServerProcess.create().use { second ->
                val ticks = TickExecutor()
                val firstManager = SessionManager(first, ticks, mapOf("flat" to Supplier { FlatSession() }))
                val secondManager = SessionManager(second, ticks, mapOf("flat" to Supplier { FlatSession() }))
                val firstEvents = mutableListOf<SessionScope>()
                val secondEvents = mutableListOf<SessionScope>()
                first.eventHandler().addListener(SessionCreateEvent::class.java) { firstEvents.add(it.session) }
                second.eventHandler().addListener(SessionCreateEvent::class.java) { secondEvents.add(it.session) }
                val session = session("flat")
                val creations =
                    listOf(firstManager.create("same-id", session), secondManager.create("same-id", session))
                repeat(4) { ticks.flush() }
                creations.forEach { it.join() }
                val firstScope = firstManager.get("same-id").scope
                val secondScope = secondManager.get("same-id").scope
                assertEquals(listOf(firstScope), firstEvents)
                assertEquals(listOf(secondScope), secondEvents)
                assertSame(first, firstScope.process)
                assertSame(second, secondScope.instances.single().process())
                val ended = firstManager.finish("same-id", session)
                repeat(8) { ticks.flush() }
                ended.join()
                first.close()
                var ran = false
                second.schedulerManager().buildTask { ran = true }.schedule()
                second.schedulerManager().process()
                assertTrue(ran)
                assertEquals(secondScope.instances.toSet(), second.instanceManager().instances)
                assertEquals(JvmSessionPhase.JVM_SESSION_PHASE_READY, secondManager.get("same-id").phase)
            }
        }
    }

    @Test
    fun `work queued after a session creation sees the created session`() {
        val process = ServerProcess.create()
        val ticks = TickExecutor()
        val manager = SessionManager(process, ticks, mapOf("flat" to Supplier { FlatSession() }))
        try {
            manager.create("queued", session("flat"))
            val queued = manager.afterQueued()
            assertTrue(manager.inventory().isEmpty())
            assertFalse(queued.isDone)
            ticks.flush()
            assertTrue(queued.isDone)
            assertEquals(listOf("queued"), manager.inventory().map { it.id })
        } finally {
            process.stop()
        }
    }

    @Test
    fun `resource disposal failure still awaits the leave hook`() {
        val process = ServerProcess.create()
        val ticks = TickExecutor()
        val left = CompletableFuture<Void>()
        var leaving = false
        val manager =
            SessionManager(
                process,
                ticks,
                mapOf(
                    "game" to
                        Supplier {
                            object : Session() {
                                override fun onCreate(scope: SessionScope) = FlatSession().onCreate(scope)

                                override fun onLeave(player: Player): CompletableFuture<Void> {
                                    leaving = true
                                    return left
                                }
                            }
                        },
                ),
            )
        val game = session("game")
        try {
            val created = manager.create("game", game)
            repeat(4) { ticks.flush() }
            created.join()
            val session = manager.get("game")
            val player =
                Player(
                    object : PlayerConnection(process) {
                        override fun sendPacket(packet: SendablePacket) {}

                        override fun getRemoteAddress() = InetSocketAddress("127.0.0.1", 0)
                    },
                    GameProfile(UUID.randomUUID(), "test"),
                )
            val joined = session.join(player)
            repeat(2) { ticks.flush() }
            joined.join()
            session.scope.own(player, AutoCloseable { error("Disposal failed") })
            val leavingResult = session.leave(player)
            ticks.flush()
            assertTrue(leaving)
            assertFalse(leavingResult.isDone)
            left.complete(null)
            ticks.flush()
            assertTrue(leavingResult.isCompletedExceptionally)
            val ended = manager.finish("game", game)
            repeat(8) { ticks.flush() }
            ended.join()
        } finally {
            left.complete(null)
            process.stop()
        }
    }

    @Test
    fun `readiness and ending await hooks and dispose only the owning session`() {
        val process = ServerProcess.create()
        val ticks = TickExecutor()
        val created = CompletableFuture<Void>()
        val finished = CompletableFuture<Void>()
        var disposed = 0
        lateinit var ownedScope: SessionScope
        val manager =
            SessionManager(
                process,
                ticks,
                mapOf(
                    "delayed" to
                        Supplier {
                            object : Session() {
                                override fun onCreate(scope: SessionScope): CompletableFuture<Void> {
                                    ownedScope = scope
                                    scope.createInstance()
                                    scope.createInstance()
                                    scope.own(AutoCloseable { disposed++ })
                                    return created
                                }

                                override fun onFinish() = finished
                            }
                        },
                    "flat" to Supplier { FlatSession() },
                    "failed" to
                        Supplier {
                            object : Session() {
                                override fun onCreate(scope: SessionScope) =
                                    CompletableFuture.failedFuture<Void>(IllegalStateException("Creation failed"))
                            }
                        },
                ),
            )

        try {
            val first = session("delayed")
            val second = session("flat").toBuilder().setCapacity(32).build()
            assertEquals(0, manager.activeCount())
            val pending = manager.create("first", first)
            val independent = manager.create("second", second)
            repeat(4) { ticks.flush() }
            assertFalse(pending.isDone)
            assertEquals(2, manager.activeCount())
            assertEquals(JvmSessionPhase.JVM_SESSION_PHASE_READY, independent.join().phase)
            assertEquals(32, independent.join().capacity)
            assertEquals(3, process.instanceManager().instances.size)
            created.complete(null)
            repeat(4) { ticks.flush() }
            assertEquals(JvmSessionPhase.JVM_SESSION_PHASE_READY, pending.join().phase)
            assertEquals(2, manager.activeCount())
            val duplicate = manager.create("first", first)
            repeat(2) { ticks.flush() }
            assertEquals(pending.join(), duplicate.join())
            val changed = manager.create("first", first.toBuilder().setCapacity(3).build())
            ticks.flush()
            assertTrue(changed.isCompletedExceptionally)
            val ending = manager.finish("first", first)
            repeat(5) { ticks.flush() }
            assertFalse(ending.isDone)
            assertEquals(2, manager.activeCount())
            assertEquals(0, disposed)
            finished.complete(null)
            repeat(5) { ticks.flush() }
            assertEquals(JvmSessionPhase.JVM_SESSION_PHASE_ENDED, ending.join().phase)
            assertEquals(1, manager.activeCount())
            assertEquals(1, disposed)
            assertEquals(1, process.instanceManager().instances.size)
            assertFalse(process.eventHandler().children.any { it.name == ownedScope.events.name })
            assertEquals(JvmSessionPhase.JVM_SESSION_PHASE_READY, manager.get("second").phase)
            val staleTask = ownedScope.onTick { error("Disposed task ran") }
            ticks.flush()
            assertTrue(staleTask.isCompletedExceptionally)
            val stopSecond = manager.finish("second", second)
            repeat(8) { ticks.flush() }
            assertTrue(stopSecond.isDone)
            assertEquals(0, manager.activeCount())
            val failed = manager.create("failed", session("failed"))
            repeat(8) { ticks.flush() }
            assertTrue(failed.isCompletedExceptionally)
            assertEquals(
                JvmSessionPhase.JVM_SESSION_PHASE_FAILED,
                manager.inventory().single { it.id == "failed" }.phase,
            )
            assertEquals(0, manager.activeCount())
            assertEquals(3, manager.inventory().size)
        } finally {
            process.stop()
        }
    }

    private fun session(type: String) =
        JvmSession
            .newBuilder()
            .setSessionType(type)
            .setCapacity(2)
            .build()
}
