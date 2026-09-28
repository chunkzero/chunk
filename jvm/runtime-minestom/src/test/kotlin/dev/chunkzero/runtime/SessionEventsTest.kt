package dev.chunkzero.runtime

import chunk.sync.v1.Jvm.JvmSession
import dev.chunkzero.runtime.minestom.event.SessionCreateEvent
import dev.chunkzero.runtime.minestom.event.SessionDestroyEvent
import dev.chunkzero.runtime.minestom.event.SessionEvent
import dev.chunkzero.runtime.minestom.event.SessionJoinEvent
import dev.chunkzero.runtime.minestom.event.SessionLeaveEvent
import net.minestom.server.ServerProcess
import net.minestom.server.coordinate.Pos
import net.minestom.server.entity.Entity
import net.minestom.server.entity.EntityType
import net.minestom.server.entity.Player
import net.minestom.server.event.Event
import net.minestom.server.event.EventNode
import net.minestom.server.event.entity.EntityTickEvent
import net.minestom.server.event.instance.InstanceRegisterEvent
import net.minestom.server.event.player.PlayerTickEvent
import net.minestom.server.event.trait.EntityEvent
import net.minestom.server.event.trait.InstanceEvent
import net.minestom.server.instance.Instance
import net.minestom.server.network.packet.server.SendablePacket
import net.minestom.server.network.player.GameProfile
import net.minestom.server.network.player.PlayerConnection
import net.minestom.server.timer.ExecutionType
import net.minestom.server.timer.TaskSchedule
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
        val create = manager.create("first", game)
        ticks.flush()
        assertTrue(global.isEmpty())
        completeOffThread(creating)
        assertTrue(global.isEmpty())
        await(create)
        val session = manager.get("first")
        assertSame(scope, global.single().session)
        assertEquals("first", global.single().session.id)

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
        await(otherManager.create("second", game))
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

        val finish = manager.finish("first", game)
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
        await(manager.finish("first", game))
        assertEquals(4, local.size)
        await(otherManager.finish("second", game))
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
        await(manager.create("first", game))
        await(manager.create("first", game))
        val session = manager.get("first")
        val player = player()
        assertThrows(CompletionException::class.java) { await(session.join(player)) }
        await(session.leave(player))
        await(manager.finish("first", game))
        assertEquals(
            listOf(SessionCreateEvent::class.java, SessionDestroyEvent::class.java),
            global.map { it.javaClass },
        )

        lateinit var partial: SessionScope
        lateinit var entity: Entity
        var runs = 0
        val failed =
            manager(
                object : Session() {
                    override fun onCreate(scope: SessionScope): CompletableFuture<Void> {
                        partial = scope
                        entity = spawn(scope.createInstance())
                        scope.scheduler
                            .buildTask { runs++ }
                            .repeat(TaskSchedule.nextTick())
                            .schedule()
                        return CompletableFuture.failedFuture(IllegalStateException("Creation failed"))
                    }
                },
            )
        assertThrows(CompletionException::class.java) { await(failed.create("failed", game)) }
        assertThrows(CompletionException::class.java) { await(failed.finish("failed", game)) }
        assertEquals(1, global.count { it.session.id == "failed" && it is SessionDestroyEvent })
        assertFalse(global.any { it.session.id == "failed" && it is SessionCreateEvent })
        process.schedulerManager().processTick()
        assertEquals(0, runs)
        assertTrue(entity.isRemoved)
        assertTrue(partial.scheduler.isClosed)
        assertTrue(process.instanceManager().instances.isEmpty())
        assertFalse(process.eventHandler().children.contains(partial.events))
    }

    @Test
    fun `sessions receive native events and run scheduled work only for what they own`() {
        val scopes = mutableListOf<SessionScope>()
        val manager =
            manager(
                object : Session() {
                    override fun onCreate(scope: SessionScope): CompletableFuture<Void> {
                        scope.createInstance()
                        scopes.add(scope)
                        return CompletableFuture.completedFuture(null)
                    }
                },
            )
        await(manager.create("first", game))
        await(manager.create("second", game))
        val (first, second) = scopes
        val firstEvents = mutableListOf<Event>()
        val secondEvents = mutableListOf<Event>()
        for ((scope, events) in listOf(first to firstEvents, second to secondEvents)) {
            scope.events.addListener(InstancePing::class.java) { events.add(it) }
            scope.events.addListener(EntityPing::class.java) { events.add(it) }
        }
        val entity = spawn(first.instances.single())
        val outsider = spawn(process.instanceManager().createInstanceContainer())
        listOf(
            InstancePing(first.instances.single()),
            EntityPing(entity),
            InstancePing(second.instances.single()),
            EntityPing(outsider),
        ).forEach(process.eventHandler()::call)
        assertEquals(2, firstEvents.size)
        assertEquals(1, secondEvents.size)

        var starts = 0
        var ends = 0
        first.scheduler
            .buildTask { starts++ }
            .repeat(TaskSchedule.nextTick())
            .schedule()
        first.scheduler
            .buildTask { ends++ }
            .repeat(TaskSchedule.nextTick())
            .executionType(ExecutionType.TICK_END)
            .schedule()
        val schedulers = process.schedulerManager()
        schedulers.processTick()
        schedulers.processTickEnd()
        assertEquals(1 to 1, starts to ends)

        await(manager.finish("first", game))
        schedulers.processTick()
        schedulers.processTickEnd()
        assertEquals(1 to 1, starts to ends)
        assertTrue(entity.isRemoved)
        assertFalse(outsider.isRemoved)
        assertEquals(setOf(second.instances.single(), outsider.instance), process.instanceManager().instances)
        await(manager.finish("second", game))
    }

    @Test
    fun `scopes observe their instance registration and owned errors do not skip teardown`() {
        lateinit var scope: SessionScope
        var registered = 0
        val manager =
            manager(
                object : Session() {
                    override fun onCreate(created: SessionScope): CompletableFuture<Void> {
                        scope = created
                        scope.events.addListener(InstanceRegisterEvent::class.java) { registered++ }
                        scope.createInstance()
                        scope.own(AutoCloseable { throw AssertionError("cleanup") })
                        return CompletableFuture.completedFuture(null)
                    }
                },
            )
        await(manager.create("owner", game))
        assertEquals(1, registered)
        assertThrows(Throwable::class.java) { await(manager.finish("owner", game)) }
        assertTrue(process.instanceManager().instances.isEmpty())
        assertFalse(process.eventHandler().children.contains(scope.events))
        assertEquals(1, global.count { it is SessionDestroyEvent })
    }

    @Test
    fun `native listeners use the scope on the process's single dispatcher thread`() {
        lateinit var scope: SessionScope
        val manager =
            manager(
                object : Session() {
                    override fun onCreate(created: SessionScope): CompletableFuture<Void> {
                        scope = created
                        spawn(scope.createInstance())
                        return CompletableFuture.completedFuture(null)
                    }
                },
            )
        await(manager.create("native", game))
        val owned = CompletableFuture<Thread>()
        scope.events.addListener(EntityTickEvent::class.java) {
            if (!owned.isDone) {
                scope.own(AutoCloseable {})
                owned.complete(Thread.currentThread())
            }
        }
        repeat(20) { if (!owned.isDone) process.ticker().tick(System.nanoTime()) }
        assertSame(process.dispatcher().threads().single(), owned.join())
        await(manager.finish("native", game))
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
        await(manager.create("first", game))
        val session = manager.get("first")
        val player = player()
        await(session.join(player))
        val leave = assertThrows(CompletionException::class.java) { await(session.leave(player)) }
        assertSame(hookFailure, leave.cause)
        assertThrows(CompletionException::class.java) { await(manager.finish("first", game)) }
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

    private val game =
        JvmSession
            .newBuilder()
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

    private fun spawn(instance: Instance): Entity {
        instance.loadChunk(0, 0).join()
        return Entity(process, EntityType.ZOMBIE).also { it.setInstance(instance, Pos(0.5, 42.0, 0.5)).join() }
    }

    private class InstancePing(
        private val target: Instance,
    ) : InstanceEvent {
        override fun getInstance() = target
    }

    private class EntityPing(
        private val target: Entity,
    ) : EntityEvent {
        override fun getEntity() = target
    }

    private fun completeOffThread(future: CompletableFuture<Void>) {
        CompletableFuture.runAsync { future.complete(null) }.join()
    }

    private fun <T> await(future: CompletableFuture<T>): T {
        repeat(20) { if (!future.isDone) ticks.flush() }
        assertTrue(future.isDone, "Lifecycle did not settle")
        return future.join()
    }
}
