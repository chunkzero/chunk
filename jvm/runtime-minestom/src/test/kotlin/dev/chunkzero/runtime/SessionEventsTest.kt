package dev.chunkzero.runtime

import chunk.v1.Common.SessionRef
import chunk.v1.Supervision.SessionCommand
import dev.chunkzero.runtime.minestom.event.SessionCreateEvent
import dev.chunkzero.runtime.minestom.event.SessionDestroyEvent
import dev.chunkzero.runtime.minestom.event.SessionEvent
import dev.chunkzero.runtime.minestom.event.SessionJoinEvent
import dev.chunkzero.runtime.minestom.event.SessionLeaveEvent
import net.minestom.server.ServerProcess
import net.minestom.server.entity.Player
import net.minestom.server.event.Event
import net.minestom.server.event.EventNode
import net.minestom.server.event.player.PlayerTickEvent
import net.minestom.server.network.packet.server.SendablePacket
import net.minestom.server.network.player.GameProfile
import net.minestom.server.network.player.PlayerConnection
import org.junit.jupiter.api.AfterEach
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Assertions.assertSame
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.BeforeEach
import org.junit.jupiter.api.Test
import java.net.InetSocketAddress
import java.util.UUID
import java.util.concurrent.CompletableFuture
import java.util.concurrent.CompletionException
import java.util.function.Supplier

class SessionEventsTest {
    private val ticks = TickExecutor()
    private val global = mutableListOf<SessionEvent>()
    private val unexpected = mutableListOf<Throwable>()
    private lateinit var process: ServerProcess

    @BeforeEach
    fun start() {
        process = ServerProcess.create()
        process.exceptionManager().setExceptionHandler { unexpected.add(it) }
        listen(process.eventHandler(), global)
    }

    @AfterEach
    fun stop() {
        try {
            assertTrue(unexpected.isEmpty(), "Unexpected listener failures: $unexpected")
        } finally {
            process.stop()
        }
    }

    @Test
    fun `events await hooks and reach only the owning session on the tick thread`() {
        val creating = CompletableFuture<Void>()
        val joining = CompletableFuture<Void>()
        val leaving = CompletableFuture<Void>()
        val finishing = CompletableFuture<Void>()
        val local = mutableListOf<SessionEvent>()
        val other = mutableListOf<SessionEvent>()
        var disposed = false
        var playerDisposed = false
        lateinit var scope: SessionScope
        val manager =
            manager(
                object : Session() {
                    override fun onCreate(created: SessionScope): CompletableFuture<Void> {
                        scope = created
                        scope.createInstance()
                        scope.own(AutoCloseable { disposed = true })
                        listen(scope.events, local)
                        scope.events.addListener(SessionDestroyEvent::class.java) {
                            assertTrue(disposed)
                            assertThrows(IllegalStateException::class.java) { scope.createInstance() }
                        }
                        return creating
                    }

                    override fun onJoin(player: Player): CompletableFuture<Void> {
                        scope.own(player, AutoCloseable { playerDisposed = true })
                        return joining
                    }

                    override fun onLeave(player: Player) = leaving

                    override fun onFinish() = finishing
                },
            )
        val create = manager.create(command("first"))
        ticks.flush()
        assertTrue(global.isEmpty())
        completeOffThread(creating)
        assertTrue(global.isEmpty())
        await(create)
        val session = manager.get("first", 1)
        assertSame(scope, global.single().session)
        assertEquals("first", global.single().session.id)
        assertEquals(1, global.single().session.generation)

        val otherManager =
            manager(
                object : Session() {
                    override fun onCreate(scope: SessionScope): CompletableFuture<Void> {
                        scope.createInstance()
                        listen(scope.events, other)
                        return CompletableFuture.completedFuture(null)
                    }
                },
            )
        await(otherManager.create(command("second")))
        val player = player()
        val join = session.join(player)
        ticks.flush()
        assertFalse(join.isDone)
        assertEquals(1, local.size)
        completeOffThread(joining)
        assertEquals(1, local.size)
        await(join)
        assertSame(player, (local.last() as SessionJoinEvent).player)

        var playerTicks = 0
        scope.events.addListener(PlayerTickEvent::class.java) { playerTicks++ }
        process.eventHandler().call(PlayerTickEvent(player))
        assertEquals(1, playerTicks)

        val leave = session.leave(player)
        ticks.flush()
        assertTrue(playerDisposed)
        assertFalse(leave.isDone)
        completeOffThread(leaving)
        await(leave)
        assertSame(player, (local.last() as SessionLeaveEvent).player)
        assertFalse(scope.players.contains(player))
        process.eventHandler().call(PlayerTickEvent(player))
        assertEquals(1, playerTicks)
        await(session.leave(player))

        val finish = manager.finish(command("first"))
        repeat(4) { ticks.flush() }
        assertFalse(disposed)
        completeOffThread(finishing)
        await(finish)
        assertEquals(
            listOf(
                SessionCreateEvent::class.java,
                SessionJoinEvent::class.java,
                SessionLeaveEvent::class.java,
                SessionDestroyEvent::class.java,
            ),
            local.map { it.javaClass },
        )
        assertEquals(local, global.filter { it.session === scope })
        assertEquals(1, other.size)
        assertFalse(process.eventHandler().children.contains(scope.events))
        await(manager.finish(command("first")))
        assertEquals(4, local.size)
        await(otherManager.finish(command("second")))
    }

    @Test
    fun `failed admission emits no join or leave and failed creation still reports disposal`() {
        val manager =
            manager(
                object : Session() {
                    override fun onCreate(scope: SessionScope): CompletableFuture<Void> {
                        scope.createInstance()
                        return CompletableFuture.completedFuture(null)
                    }

                    override fun onJoin(player: Player) =
                        CompletableFuture.failedFuture<Void>(IllegalStateException("Join failed"))
                },
            )
        await(manager.create(command("first")))
        await(manager.create(command("first")))
        val session = manager.get("first", 1)
        val player = player()
        assertThrows(CompletionException::class.java) { await(session.join(player)) }
        await(session.leave(player))
        await(manager.finish(command("first")))
        assertEquals(
            listOf(SessionCreateEvent::class.java, SessionDestroyEvent::class.java),
            global.map { it.javaClass },
        )

        val failed =
            manager(
                object : Session() {
                    override fun onCreate(scope: SessionScope) =
                        CompletableFuture.failedFuture<Void>(IllegalStateException("Creation failed"))
                },
            )
        assertThrows(CompletionException::class.java) { await(failed.create(command("failed"))) }
        assertThrows(CompletionException::class.java) { await(failed.finish(command("failed"))) }
        assertEquals(1, global.count { it.session.id == "failed" && it is SessionDestroyEvent })
        assertFalse(global.any { it.session.id == "failed" && it is SessionCreateEvent })
    }

    @Test
    fun `listener failures do not fail admission or prevent cleanup and hook failures remain visible`() {
        val listenerFailure = IllegalArgumentException("Listener failed")
        val reported = mutableListOf<Throwable>()
        process.exceptionManager().setExceptionHandler { reported.add(it) }
        val hookFailure = IllegalStateException("Leave failed")
        val manager =
            manager(
                object : Session() {
                    override fun onCreate(scope: SessionScope): CompletableFuture<Void> {
                        scope.createInstance()
                        scope.own(AutoCloseable { error("Disposal failed") })
                        scope.events.addListener(SessionJoinEvent::class.java) { throw listenerFailure }
                        scope.events.addListener(SessionLeaveEvent::class.java) { throw listenerFailure }
                        scope.events.addListener(SessionDestroyEvent::class.java) { throw listenerFailure }
                        return CompletableFuture.completedFuture(null)
                    }

                    override fun onLeave(player: Player) = CompletableFuture.failedFuture<Void>(hookFailure)
                },
            )
        await(manager.create(command("first")))
        val session = manager.get("first", 1)
        val player = player()
        await(session.join(player))
        val leave = assertThrows(CompletionException::class.java) { await(session.leave(player)) }
        assertSame(hookFailure, leave.cause)
        assertThrows(CompletionException::class.java) { await(manager.finish(command("first"))) }
        assertEquals(3, reported.size)
        assertTrue(reported.all { it === listenerFailure })
        assertTrue(global.last() is SessionDestroyEvent)
        assertFalse(process.eventHandler().children.contains(session.scope.events))
        assertTrue(process.instanceManager().instances.isEmpty())
    }

    private fun listen(
        node: EventNode<Event>,
        events: MutableList<SessionEvent>,
    ) {
        fun record(event: SessionEvent) {
            ticks.checkThread()
            events.add(event)
        }
        node.addListener(SessionCreateEvent::class.java, ::record)
        node.addListener(SessionJoinEvent::class.java, ::record)
        node.addListener(SessionLeaveEvent::class.java, ::record)
        node.addListener(SessionDestroyEvent::class.java, ::record)
    }

    private fun manager(session: Session) = SessionManager(process, ticks, mapOf("game" to Supplier { session }))

    private fun command(id: String) =
        SessionCommand
            .newBuilder()
            .setOperationId(id)
            .setSession(SessionRef.newBuilder().setId(id))
            .setGeneration(1)
            .setSessionType("game")
            .setCapacity(2)
            .build()

    private fun player() =
        Player(
            object : PlayerConnection(process) {
                override fun sendPacket(packet: SendablePacket) {}

                override fun getRemoteAddress() = InetSocketAddress("127.0.0.1", 0)
            },
            GameProfile(UUID.randomUUID(), "test"),
        )

    private fun completeOffThread(future: CompletableFuture<Void>) {
        CompletableFuture.runAsync { future.complete(null) }.join()
    }

    private fun <T> await(future: CompletableFuture<T>): T {
        repeat(20) { if (!future.isDone) ticks.flush() }
        assertTrue(future.isDone, "Lifecycle did not settle")
        return future.join()
    }
}
