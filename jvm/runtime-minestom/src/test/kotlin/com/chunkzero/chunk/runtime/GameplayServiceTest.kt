package com.chunkzero.chunk.runtime

import chunk.sync.v1.CoreOuterClass.Position
import chunk.sync.v1.Gateway.PlayerIdentity
import chunk.sync.v1.Gateway.PlayerProperty
import chunk.sync.v1.Jvm.JvmDelivery
import chunk.sync.v1.Jvm.JvmDeliveryPhase
import chunk.sync.v1.Jvm.JvmSession
import chunk.sync.v1.Jvm.JvmSessionPhase
import chunk.sync.v1.Jvm.PlayerSetup
import com.chunkzero.chunk.runtime.bootstrap.FlatSession
import com.chunkzero.chunk.runtime.minestom.internal.GameplayService
import com.chunkzero.chunk.runtime.minestom.internal.ProcessService
import com.chunkzero.chunk.runtime.minestom.internal.SessionMethodService
import com.google.protobuf.ByteString
import net.minestom.server.ServerProcess
import net.minestom.server.network.ConnectionState
import net.minestom.server.network.packet.server.login.LoginDisconnectPacket
import net.minestom.server.network.packet.server.login.LoginSuccessPacket
import net.minestom.server.timer.TaskSchedule
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import java.net.InetSocketAddress
import java.net.Socket
import java.util.UUID
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicLong
import java.util.function.Supplier

class GameplayServiceTest {
    @Test
    fun `topic deliveries admit their player once and close when withdrawn, gone or expired`() {
        val minecraft = ServerProcess.create()
        minecraft.setCompressionThreshold(0)
        minecraft.connectionManager().setPlayerProvider(::ManagedPlayer)
        val ticks = TickExecutor()
        val manager = SessionManager(minecraft, ticks, mapOf("bridge" to Supplier { FlatSession() }))
        val clock = AtomicLong(System.nanoTime())
        val gameplay = GameplayService(manager, clock::get) { true }
        val methods = SessionMethodService(manager, mapOf(), gameplay::arrived, { _, _ -> }, System::currentTimeMillis)
        val core = FakeCore()
        minecraft
            .schedulerManager()
            .buildTask {
                ticks.flush()
                gameplay.flush()
                core.wake()
            }.repeat(TaskSchedule.tick(1))
            .schedule()
        minecraft.start(InetSocketAddress("127.0.0.1", 0))
        val port = minecraft.server().port
        val sockets = mutableListOf<Socket>()
        try {
            core.connect(ProcessService(manager, { true }, gameplay, methods))
            core.put(
                "session/bridge",
                JvmSession
                    .newBuilder()
                    .setSessionType("bridge")
                    .setCapacity(128)
                    .build(),
            )
            val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(10)
            while (manager.phase("bridge") != JvmSessionPhase.JVM_SESSION_PHASE_READY) {
                check(System.nanoTime() < deadline) { "The session never became ready" }
                Thread.sleep(10)
            }
            val uuid = UUID.randomUUID()

            fun delivery(revision: Long) =
                JvmDelivery
                    .newBuilder()
                    .setSession("bridge")
                    .setGeneration(Position.newBuilder().setEpoch(1).setRevision(revision))
                    .setPlayer(
                        PlayerIdentity
                            .newBuilder()
                            .setUuid(uuid.toString())
                            .setUsername("player")
                            .addProperties(
                                PlayerProperty
                                    .newBuilder()
                                    .setName("textures")
                                    .setValue("value")
                                    .setSignature("signature"),
                            ),
                    ).build()

            fun prepare(
                operation: String,
                revision: Long,
            ): PlayerSetup {
                core.put("delivery/$operation", delivery(revision))
                val prepared = core.await(operation, JvmDeliveryPhase.JVM_DELIVERY_PHASE_PREPARED)
                assertEquals(32, prepared.capability.size())
                return PlayerSetup
                    .newBuilder()
                    .setOperationId(operation)
                    .setCapability(prepared.capability)
                    .build()
            }

            fun attempt(
                setup: PlayerSetup,
                name: String = "player",
            ) = login(port, name, uuid, setup).also { sockets.add(it) }

            fun arrive(setup: PlayerSetup): Socket {
                val socket = attempt(setup)
                val success = socket.packet(ConnectionState.LOGIN) as LoginSuccessPacket
                assertEquals(
                    "signature",
                    success
                        .gameProfile()
                        .properties()
                        .single()
                        .signature(),
                )
                socket.configure()
                socket.confirmTeleports()
                core.await(setup.operationId, JvmDeliveryPhase.JVM_DELIVERY_PHASE_ARRIVED)
                return socket
            }

            val first = prepare("first", 1)
            for (invalid in listOf(
                first.toBuilder().setCapability(ByteString.EMPTY).build(),
                first.toBuilder().setOperationId("unknown").build(),
            )) {
                assertTrue(attempt(invalid).packet(ConnectionState.LOGIN) is LoginDisconnectPacket)
            }
            assertTrue(attempt(first, "other").packet(ConnectionState.LOGIN) is LoginDisconnectPacket)
            arrive(first)
            // The capability admits its player once.
            assertTrue(attempt(first).packet(ConnectionState.LOGIN) is LoginDisconnectPacket)
            core.put("delivery/first", delivery(1).toBuilder().setWithdraw(true).build())
            core.await("first", JvmDeliveryPhase.JVM_DELIVERY_PHASE_CLOSED)
            val phases = core.phases("first")
            assertEquals(JvmDeliveryPhase.JVM_DELIVERY_PHASE_PREPARED, phases.first())
            assertTrue(JvmDeliveryPhase.JVM_DELIVERY_PHASE_ARRIVED in phases)
            assertEquals(phases.sortedBy { it.number }, phases, "Phases only move forward")

            // A delivery whose key disappears closes as if withdrawn.
            arrive(prepare("second", 2))
            core.remove("delivery/second")
            core.await("second", JvmDeliveryPhase.JVM_DELIVERY_PHASE_CLOSED)
            assertTrue(minecraft.connectionManager().onlinePlayers.isEmpty())

            val expired = prepare("expired", 3)
            clock.addAndGet(TimeUnit.SECONDS.toNanos(61))
            core.await("expired", JvmDeliveryPhase.JVM_DELIVERY_PHASE_CLOSED)
            assertTrue(attempt(expired).packet(ConnectionState.LOGIN) is LoginDisconnectPacket)
        } finally {
            sockets.forEach { it.close() }
            core.close()
            gameplay.close()
            minecraft.stop()
        }
    }
}
