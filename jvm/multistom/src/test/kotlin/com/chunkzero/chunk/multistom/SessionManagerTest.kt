package com.chunkzero.chunk.multistom

import chunk.sync.v1.Jvm.JvmSession
import chunk.sync.v1.Jvm.JvmSessionPhase
import com.chunkzero.chunk.multistom.bootstrap.FlatSession
import com.chunkzero.chunk.multistom.event.SessionCreateEvent
import com.chunkzero.chunk.runtime.TestHosts
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
                val firstHost = TestHosts.detached(firstManager)
                val secondHost = TestHosts.detached(secondManager)
                val firstEvents = mutableListOf<SessionScope>()
                val secondEvents = mutableListOf<SessionScope>()
                first.eventHandler().addListener(SessionCreateEvent::class.java) { firstEvents.add(it.session) }
                second.eventHandler().addListener(SessionCreateEvent::class.java) { secondEvents.add(it.session) }
                val session = session("flat")
                val creations =
                    listOf(
                        TestHosts.create(firstHost, "same-id", session),
                        TestHosts.create(secondHost, "same-id", session),
                    )
                repeat(4) { ticks.flush() }
                creations.forEach { it.join() }
                val firstScope = firstManager.get("same-id").scope
                val secondScope = secondManager.get("same-id").scope
                assertEquals(listOf(firstScope), firstEvents)
                assertEquals(listOf(secondScope), secondEvents)
                assertSame(first, firstScope.process)
                assertSame(second, secondScope.instances.single().process())
                val ended = TestHosts.finish(firstHost, "same-id", session)
                repeat(8) { ticks.flush() }
                ended.join()
                first.close()
                var ran = false
                second.schedulerManager().buildTask { ran = true }.schedule()
                second.schedulerManager().process()
                assertTrue(ran)
                assertEquals(secondScope.instances.toSet(), second.instanceManager().instances)
                secondManager.get("same-id")
            }
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
        val host = TestHosts.detached(manager)
        try {
            val created = TestHosts.create(host, "game", game)
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
            val ended = TestHosts.finish(host, "game", game)
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

        val host = TestHosts.detached(manager)
        try {
            val first = session("delayed")
            val second = session("flat").toBuilder().setCapacity(32).build()
            assertEquals(0, host.activeCount())
            val pending = TestHosts.create(host, "first", first)
            val independent = TestHosts.create(host, "second", second)
            repeat(4) { ticks.flush() }
            assertFalse(pending.isDone)
            assertEquals(2, host.activeCount())
            assertEquals(JvmSessionPhase.JVM_SESSION_PHASE_READY, independent.join().phase)
            assertEquals(32, independent.join().capacity)
            assertEquals(3, process.instanceManager().instances.size)
            created.complete(null)
            repeat(4) { ticks.flush() }
            assertEquals(JvmSessionPhase.JVM_SESSION_PHASE_READY, pending.join().phase)
            assertEquals(2, host.activeCount())
            val duplicate = TestHosts.create(host, "first", first)
            repeat(2) { ticks.flush() }
            assertEquals(pending.join(), duplicate.join())
            val changed = TestHosts.create(host, "first", first.toBuilder().setCapacity(3).build())
            ticks.flush()
            assertTrue(changed.isCompletedExceptionally)
            val ending = TestHosts.finish(host, "first", first)
            repeat(5) { ticks.flush() }
            assertFalse(ending.isDone)
            assertEquals(2, host.activeCount())
            assertEquals(0, disposed)
            finished.complete(null)
            repeat(5) { ticks.flush() }
            assertEquals(JvmSessionPhase.JVM_SESSION_PHASE_ENDED, ending.join().phase)
            assertEquals(1, host.activeCount())
            assertEquals(1, disposed)
            assertEquals(1, process.instanceManager().instances.size)
            assertFalse(process.eventHandler().children.any { it.name == ownedScope.events.name })
            manager.get("second")
            val staleTask = ownedScope.onTick { error("Disposed task ran") }
            ticks.flush()
            assertTrue(staleTask.isCompletedExceptionally)
            val stopSecond = TestHosts.finish(host, "second", second)
            repeat(8) { ticks.flush() }
            assertTrue(stopSecond.isDone)
            assertEquals(0, host.activeCount())
            val failed = TestHosts.create(host, "failed", session("failed"))
            repeat(8) { ticks.flush() }
            assertTrue(failed.isCompletedExceptionally)
            assertEquals(
                JvmSessionPhase.JVM_SESSION_PHASE_FAILED,
                TestHosts
                    .inventory(host)
                    .sessionsList
                    .single { it.id == "failed" }
                    .phase,
            )
            assertEquals(0, host.activeCount())
            assertEquals(3, TestHosts.inventory(host).sessionsList.size)
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
