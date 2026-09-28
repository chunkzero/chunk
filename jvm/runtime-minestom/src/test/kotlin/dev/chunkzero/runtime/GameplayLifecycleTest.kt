package dev.chunkzero.runtime

import chunk.sync.v1.CoreOuterClass.Position
import chunk.sync.v1.Gateway.PlayerIdentity
import chunk.sync.v1.Jvm.JvmDelivery
import chunk.sync.v1.Jvm.JvmDeliveryPhase
import chunk.sync.v1.Jvm.JvmSession
import chunk.sync.v1.Jvm.PlayerSetup
import dev.chunkzero.runtime.bootstrap.FlatSession
import dev.chunkzero.runtime.minestom.internal.GameplayService
import net.minestom.server.ServerProcess
import net.minestom.server.entity.Player
import net.minestom.server.network.ConnectionState
import net.minestom.server.network.packet.server.login.LoginSuccessPacket
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import java.net.InetSocketAddress
import java.net.Socket
import java.util.UUID
import java.util.concurrent.CompletableFuture
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.TimeUnit
import java.util.function.Supplier

class GameplayLifecycleTest {
    @Test
    fun `withdrawal fences UUID reuse and leaves the other session running`() {
        val minecraft = ServerProcess.create()
        minecraft.setCompressionThreshold(0)
        minecraft.connectionManager().setPlayerProvider(::ManagedPlayer)
        val ticks = TickExecutor()
        val closedPlayers = ConcurrentHashMap.newKeySet<Player>()
        val joinStarted = CompletableFuture<Unit>()
        val joinFinished = CompletableFuture<Void>()
        val manager =
            SessionManager(
                minecraft,
                ticks,
                mapOf(
                    "flat" to
                        Supplier {
                            object : Session() {
                                lateinit var scope: SessionScope

                                override fun onCreate(scope: SessionScope) =
                                    FlatSession().onCreate(scope).also { this.scope = scope }

                                override fun onJoin(player: Player): CompletableFuture<Void> {
                                    scope.own(player, AutoCloseable { closedPlayers.add(player) })
                                    return CompletableFuture.completedFuture(null)
                                }
                            }
                        },
                    "gated" to
                        Supplier {
                            object : Session() {
                                override fun onCreate(scope: SessionScope) = FlatSession().onCreate(scope)

                                override fun onJoin(player: Player): CompletableFuture<Void> {
                                    assertTrue(player.isOnline)
                                    joinStarted.complete(Unit)
                                    return joinFinished
                                }
                            }
                        },
                ),
            )
        val service = GameplayService(manager, System::nanoTime) { true }
        val sockets = mutableListOf<Socket>()
        minecraft
            .schedulerManager()
            .buildTask {
                ticks.flush()
                service.flush()
            }.repeat(
                net.minestom.server.timer.TaskSchedule
                    .tick(1),
            ).schedule()
        minecraft.start(InetSocketAddress("127.0.0.1", 0))
        try {
            fun session(type: String = "flat") =
                JvmSession
                    .newBuilder()
                    .setSessionType(type)
                    .setCapacity(2)
                    .build()
            manager.create("a", session()).get(3, TimeUnit.SECONDS)
            manager.create("b", session()).get(3, TimeUnit.SECONDS)
            val uuid = UUID.randomUUID().toString()
            val wanted = mutableMapOf<String, JvmDelivery>()

            fun delivery(
                session: String,
                generation: Long,
            ) = JvmDelivery
                .newBuilder()
                .setSession(session)
                .setGeneration(Position.newBuilder().setEpoch(1).setRevision(generation))
                .setPlayer(PlayerIdentity.newBuilder().setUuid(uuid).setUsername("test"))
                .build()

            fun phase(operation: String) = service.deliveries().single { it.operationId == operation }.phase

            fun await(
                operation: String,
                phase: JvmDeliveryPhase,
            ) {
                val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(10)
                while (phase(operation) != phase) {
                    check(System.nanoTime() < deadline) { "$operation never reached $phase" }
                    Thread.sleep(10)
                }
            }

            fun connect(
                operation: String,
                request: JvmDelivery,
            ): Socket {
                wanted[operation] = request
                service.apply(wanted)
                val prepared = service.deliveries().single { it.operationId == operation }
                val setup =
                    PlayerSetup
                        .newBuilder()
                        .setOperationId(operation)
                        .setCapability(prepared.capability)
                        .build()
                val socket = login(minecraft.server().port, "test", UUID.fromString(uuid), setup)
                sockets.add(socket)
                check(socket.packet(ConnectionState.LOGIN) is LoginSuccessPacket) { "Admission rejected" }
                socket.configure()
                return socket
            }

            fun arrive(
                socket: Socket,
                operation: String,
            ) {
                socket.confirmTeleports()
                await(operation, JvmDeliveryPhase.JVM_DELIVERY_PHASE_ARRIVED)
            }

            fun withdraw(operation: String) {
                wanted[operation] =
                    wanted
                        .getValue(operation)
                        .toBuilder()
                        .setWithdraw(true)
                        .build()
                service.apply(wanted)
            }
            arrive(connect("first", delivery("a", 1)), "first")
            assertTrue(service.arrived("first", "a"))
            assertFalse(service.arrived("first", "b"))
            val oldPlayer = minecraft.connectionManager().onlinePlayers.single()
            assertThrows(IllegalStateException::class.java) { connect("destination", delivery("b", 2)) }
            withdraw("first")
            await("first", JvmDeliveryPhase.JVM_DELIVERY_PHASE_CLOSED)
            assertFalse(service.arrived("first", "a"))
            assertTrue(oldPlayer.isRemoved)
            assertTrue(oldPlayer in closedPlayers)
            assertTrue(minecraft.connectionManager().onlinePlayers.isEmpty())
            arrive(connect("next", delivery("b", 3)), "next")
            manager.finish("a", session()).get(3, TimeUnit.SECONDS)
            val current = minecraft.connectionManager().onlinePlayers.single()
            assertEquals(uuid, current.uuid.toString())
            assertTrue(
                manager
                    .get("b")
                    .scope.instances
                    .contains(current.instance),
            )
            assertTrue(current.isOnline)
            assertEquals(setOf(oldPlayer), closedPlayers)
            withdraw("next")
            await("next", JvmDeliveryPhase.JVM_DELIVERY_PHASE_CLOSED)
            manager.create("c", session("gated")).get(3, TimeUnit.SECONDS)
            connect("pending", delivery("c", 4))
            joinStarted.get(3, TimeUnit.SECONDS)
            assertTrue(phase("pending") != JvmDeliveryPhase.JVM_DELIVERY_PHASE_ARRIVED)
            withdraw("pending")
            assertThrows(IllegalStateException::class.java) { connect("conflicting", delivery("c", 5)) }
            assertEquals(
                JvmDeliveryPhase.JVM_DELIVERY_PHASE_WITHDRAWING,
                phase("pending"),
                "Withdrawal must await the old asynchronous join",
            )
            joinFinished.complete(null)
            await("pending", JvmDeliveryPhase.JVM_DELIVERY_PHASE_CLOSED)
            arrive(connect("replacement", delivery("c", 6)), "replacement")
            withdraw("replacement")
            await("replacement", JvmDeliveryPhase.JVM_DELIVERY_PHASE_CLOSED)
            manager.finish("c", session("gated")).get(3, TimeUnit.SECONDS)
            manager.finish("b", session()).get(3, TimeUnit.SECONDS)
        } finally {
            sockets.forEach { it.close() }
            service.close()
            minecraft.stop()
        }
    }
}
