package dev.chunkzero.runtime

import chunk.v1.Common.SessionRef
import chunk.v1.Supervision.SessionCommand
import chunk.v1.Supervision.SessionPhase
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
                val command =
                    SessionCommand
                        .newBuilder()
                        .setOperationId("create")
                        .setSession(SessionRef.newBuilder().setId("same-id"))
                        .setGeneration(1)
                        .setSessionType("flat")
                        .setCapacity(2)
                        .build()
                val creations = listOf(firstManager.create(command), secondManager.create(command))
                repeat(4) { ticks.flush() }
                creations.forEach { it.join() }
                val firstScope = firstManager.get("same-id", 1).scope
                val secondScope = secondManager.get("same-id", 1).scope
                assertEquals(listOf(firstScope), firstEvents)
                assertEquals(listOf(secondScope), secondEvents)
                assertSame(first, firstScope.process)
                assertSame(second, secondScope.instances.single().process())
                val ended = firstManager.finish(command)
                repeat(8) { ticks.flush() }
                ended.join()
                first.close()
                var ran = false
                second.schedulerManager().buildTask { ran = true }.schedule()
                second.schedulerManager().process()
                assertTrue(ran)
                assertEquals(secondScope.instances.toSet(), second.instanceManager().instances)
                assertEquals(SessionPhase.SESSION_PHASE_READY, secondManager.get("same-id", 1).phase)
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
        val command =
            SessionCommand
                .newBuilder()
                .setOperationId("game")
                .setSession(SessionRef.newBuilder().setId("game"))
                .setGeneration(1)
                .setSessionType("game")
                .setCapacity(2)
                .build()
        try {
            val created = manager.create(command)
            repeat(4) { ticks.flush() }
            created.join()
            val session = manager.get("game", 1)
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
            val ended = manager.finish(command)
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

        fun command(
            id: String,
            type: String,
        ) = SessionCommand
            .newBuilder()
            .setOperationId(id)
            .setSession(SessionRef.newBuilder().setId(id))
            .setGeneration(1)
            .setSessionType(type)
            .setCapacity(2)
            .build()
        try {
            val first = command("first", "delayed")
            val second = command("second", "flat").toBuilder().setCapacity(32).build()
            assertEquals(0, manager.activeCount())
            val pending = manager.create(first)
            val independent = manager.create(second)
            repeat(4) { ticks.flush() }
            assertFalse(pending.isDone)
            assertEquals(2, manager.activeCount())
            assertEquals(SessionPhase.SESSION_PHASE_READY, independent.join().phase)
            assertEquals(32, independent.join().capacity)
            assertEquals(3, process.instanceManager().instances.size)
            created.complete(null)
            repeat(4) { ticks.flush() }
            assertEquals(SessionPhase.SESSION_PHASE_READY, pending.join().phase)
            assertEquals(2, manager.activeCount())
            val duplicate = manager.create(first)
            repeat(2) { ticks.flush() }
            assertEquals(pending.join(), duplicate.join())
            val changed = manager.create(first.toBuilder().setCapacity(3).build())
            ticks.flush()
            assertTrue(changed.isCompletedExceptionally)
            val ending = manager.finish(first)
            repeat(5) { ticks.flush() }
            assertFalse(ending.isDone)
            assertEquals(2, manager.activeCount())
            assertEquals(0, disposed)
            finished.complete(null)
            repeat(5) { ticks.flush() }
            assertEquals(SessionPhase.SESSION_PHASE_ENDED, ending.join().phase)
            assertEquals(1, manager.activeCount())
            assertEquals(1, disposed)
            assertEquals(1, process.instanceManager().instances.size)
            assertFalse(process.eventHandler().children.any { it.name == ownedScope.events.name })
            assertEquals(SessionPhase.SESSION_PHASE_READY, manager.get("second", 1).phase)
            val staleTask = ownedScope.onTick { error("Disposed task ran") }
            ticks.flush()
            assertTrue(staleTask.isCompletedExceptionally)
            val stopSecond = manager.finish(second)
            repeat(8) { ticks.flush() }
            assertTrue(stopSecond.isDone)
            assertEquals(0, manager.activeCount())
            val failed = manager.create(command("failed", "failed"))
            repeat(8) { ticks.flush() }
            assertTrue(failed.isCompletedExceptionally)
            assertEquals(
                SessionPhase.SESSION_PHASE_FAILED,
                manager.inventory().single { it.session.id == "failed" }.phase,
            )
            assertEquals(0, manager.activeCount())
            assertEquals(3, manager.inventory().size)
        } finally {
            process.stop()
        }
    }
}
