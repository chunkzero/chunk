package com.chunkzero.chunk.runtime

import chunk.sync.v1.CoreOuterClass.Position
import chunk.sync.v1.Gateway.PlayerIdentity
import chunk.sync.v1.Jvm.JvmDelivery
import chunk.sync.v1.Jvm.JvmDeliveryPhase
import chunk.sync.v1.Jvm.JvmMethodCall
import chunk.sync.v1.Jvm.JvmMethodPhase
import chunk.sync.v1.Jvm.JvmSession
import chunk.sync.v1.Jvm.JvmSessionPhase
import chunk.sync.v1.Jvm.PlayerSetup
import com.chunkzero.chunk.backend.api.BackendValues
import com.chunkzero.chunk.backend.api.JsonType
import com.chunkzero.chunk.backend.api.SessionMethodRef
import com.chunkzero.chunk.runtime.bootstrap.FlatSession
import com.chunkzero.chunk.runtime.minestom.internal.GameplayService
import com.google.protobuf.ByteString
import net.minestom.server.ServerProcess
import net.minestom.server.network.ConnectionState
import net.minestom.server.network.packet.server.login.LoginSuccessPacket
import net.minestom.server.timer.TaskSchedule
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import tools.jackson.core.type.TypeReference
import java.net.InetSocketAddress
import java.util.UUID
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger
import java.util.function.Supplier

class SessionMethodDisconnectTest {
    @Test
    fun `a method queued for a player who disconnected before its tick is cancelled`() {
        val minecraft = ServerProcess.create()
        minecraft.setCompressionThreshold(0)
        minecraft.connectionManager().setPlayerProvider(::ManagedPlayer)
        val ticks = TickExecutor()
        val runs = AtomicInteger()
        val number = JsonType.of(object : TypeReference<Long>() {}, BackendValues::checkInteger)
        val binding =
            SessionMethodBinding(
                SessionMethodRef("lobby", "default", "record", number, number),
            ) { _, input -> runs.incrementAndGet().toLong() + input }
        val manager =
            SessionManager(
                minecraft,
                ticks,
                mapOf("lobby/default" to SessionRegistration("lobby", Supplier { FlatSession() })),
                mapOf("lobby/default/record" to binding),
            )
        val core = FakeCore()
        val host = TestHosts.linked(manager, core, System::nanoTime)
        val gameplay = GameplayService(manager, host)

        // Pausing gameplay.flush keeps the disconnect unnoticed, as it is until a tick's flush.
        val paused = AtomicBoolean()
        minecraft
            .schedulerManager()
            .buildTask {
                ticks.flush()
                if (!paused.get()) gameplay.flush()
                host.sweep()
            }.repeat(TaskSchedule.tick(1))
            .schedule()
        minecraft.start(InetSocketAddress("127.0.0.1", 0))
        val socket =
            try {
                core.connect(host.state())
                core.put(
                    "session/lobby",
                    JvmSession
                        .newBuilder()
                        .setSessionType("lobby/default")
                        .setCapacity(2)
                        .build(),
                )
                val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(10)
                while (host.phase("lobby") != JvmSessionPhase.JVM_SESSION_PHASE_READY) {
                    check(System.nanoTime() < deadline) { "The session never became ready" }
                    Thread.sleep(10)
                }
                val uuid = UUID.randomUUID()
                core.put(
                    "delivery/first",
                    JvmDelivery
                        .newBuilder()
                        .setSession("lobby")
                        .setGeneration(Position.newBuilder().setEpoch(1).setRevision(1))
                        .setPlayer(PlayerIdentity.newBuilder().setUuid(uuid.toString()).setUsername("player"))
                        .build(),
                )
                val prepared = core.await("first", JvmDeliveryPhase.JVM_DELIVERY_PHASE_PREPARED)
                val setup =
                    PlayerSetup
                        .newBuilder()
                        .setOperationId("first")
                        .setCapability(prepared.capability)
                        .build()
                login(minecraft.server().port, "player", uuid, setup).also {
                    check(it.packet(ConnectionState.LOGIN) is LoginSuccessPacket)
                    it.configure()
                    it.confirmTeleports()
                    core.await("first", JvmDeliveryPhase.JVM_DELIVERY_PHASE_ARRIVED)
                }
            } catch (error: Throwable) {
                core.close()
                gameplay.close()
                minecraft.stop()
                throw error
            }
        try {
            val player = minecraft.connectionManager().onlinePlayers.single()
            paused.set(true)
            socket.close()
            val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5)
            while (player.playerConnection.isOnline) {
                check(System.nanoTime() < deadline) { "The disconnect was never noticed" }
                Thread.sleep(10)
            }
            core.put(
                "method/call",
                JvmMethodCall
                    .newBuilder()
                    .setSession("lobby")
                    .setMethod("record")
                    .setArgumentsJson(ByteString.copyFromUtf8("1"))
                    .setDelivery("first")
                    .setDeadlineMs(System.currentTimeMillis() + 30_000)
                    .build(),
            )
            assertEquals(JvmMethodPhase.JVM_METHOD_PHASE_CANCELLED, core.result("call") {}.phase)
            assertEquals(0, runs.get())
            assertTrue(!host.delivery("first")!!.isArrived)
        } finally {
            socket.close()
            core.close()
            gameplay.close()
            minecraft.stop()
        }
    }
}
