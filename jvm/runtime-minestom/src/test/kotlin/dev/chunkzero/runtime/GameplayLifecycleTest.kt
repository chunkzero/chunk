package dev.chunkzero.runtime

import chunk.v1.Common.DeploymentRef
import chunk.v1.Common.Identity
import chunk.v1.Common.PlayerRef
import chunk.v1.Common.SessionRef
import chunk.v1.GameplayGrpc
import chunk.v1.GameplayOuterClass.PlayerDelivery
import chunk.v1.GameplayOuterClass.PlayerSetup
import chunk.v1.GameplayOuterClass.PlayerWithdrawal
import chunk.v1.Supervision.DeliveryPhase
import chunk.v1.Supervision.SessionCommand
import dev.chunkzero.runtime.bootstrap.FlatSession
import dev.chunkzero.runtime.delivery.GameplayService
import io.grpc.netty.shaded.io.grpc.netty.NettyChannelBuilder
import io.grpc.netty.shaded.io.grpc.netty.NettyServerBuilder
import net.minestom.server.MinecraftServer
import net.minestom.server.entity.Player
import net.minestom.server.network.ConnectionState
import net.minestom.server.network.packet.client.common.ClientSettingsPacket
import net.minestom.server.network.packet.client.configuration.ClientFinishConfigurationPacket
import net.minestom.server.network.packet.client.configuration.ClientSelectKnownPacksPacket
import net.minestom.server.network.packet.client.handshake.ClientHandshakePacket
import net.minestom.server.network.packet.client.login.ClientLoginAcknowledgedPacket
import net.minestom.server.network.packet.client.login.ClientLoginPluginResponsePacket
import net.minestom.server.network.packet.client.login.ClientLoginStartPacket
import net.minestom.server.network.packet.client.play.ClientTeleportConfirmPacket
import net.minestom.server.network.packet.server.configuration.FinishConfigurationPacket
import net.minestom.server.network.packet.server.configuration.SelectKnownPacksPacket
import net.minestom.server.network.packet.server.login.LoginPluginRequestPacket
import net.minestom.server.network.packet.server.login.LoginSuccessPacket
import net.minestom.server.network.packet.server.play.PlayerPositionAndLookPacket
import net.minestom.server.network.player.ClientSettings
import org.junit.jupiter.api.Assertions.assertEquals
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
        val minecraft = MinecraftServer.init()
        MinecraftServer.setCompressionThreshold(0)
        MinecraftServer.getConnectionManager().setPlayerProvider(::ManagedPlayer)
        val ticks = TickExecutor()
        val closedPlayers = ConcurrentHashMap.newKeySet<Player>()
        val joinStarted = CompletableFuture<Unit>()
        val joinFinished = CompletableFuture<Void>()
        val manager =
            SessionManager(
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
        val deployment =
            DeploymentRef
                .newBuilder()
                .setEnvironment("local")
                .setDeployment("test")
                .build()
        val service = GameplayService(deployment, 1, manager, System::nanoTime, "bridge")
        val server =
            NettyServerBuilder
                .forAddress(
                    InetSocketAddress("127.0.0.1", 0),
                ).addService(service)
                .build()
                .start()
        val channel = NettyChannelBuilder.forAddress("127.0.0.1", server.port).usePlaintext().build()
        val sockets = mutableListOf<Socket>()
        MinecraftServer
            .getSchedulerManager()
            .buildTask {
                ticks.flush()
                service.flush()
            }.repeat(
                net.minestom.server.timer.TaskSchedule
                    .tick(1),
            ).schedule()
        minecraft.start("127.0.0.1", 0)
        service.endpoint = "127.0.0.1:${MinecraftServer.process().server().port}"
        try {
            fun command(id: String) =
                SessionCommand
                    .newBuilder()
                    .setOperationId(id)
                    .setSession(
                        SessionRef.newBuilder().setId(id),
                    ).setGeneration(1)
                    .setSessionType("flat")
                    .setCapacity(2)
                    .build()
            manager.create(command("a")).get(3, TimeUnit.SECONDS)
            manager.create(command("b")).get(3, TimeUnit.SECONDS)

            fun stub() = GameplayGrpc.newBlockingStub(channel).withDeadlineAfter(5, TimeUnit.SECONDS)
            val uuid = UUID.randomUUID().toString()

            fun delivery(
                session: String,
                generation: Long,
            ) = PlayerDelivery
                .newBuilder()
                .setDeployment(deployment)
                .setProcessGeneration(1)
                .setRuntimeId("bridge")
                .setOperationId("delivery-$generation")
                .setSession(SessionRef.newBuilder().setId(session))
                .setSessionGeneration(1)
                .setMembershipGeneration(1)
                .setProxyId("test-proxy")
                .setConnectionId("test-connection")
                .setOwnerGeneration(generation)
                .setPlayer(PlayerRef.newBuilder().setId(uuid))
                .setIdentity(Identity.newBuilder().setUuid(uuid).setUsername("test"))
                .setProtocol(
                    MinecraftServer.PROTOCOL_VERSION,
                ).build()

            fun connect(request: PlayerDelivery): Socket {
                val prepared = stub().preparePlayer(request)
                val socket = Socket("127.0.0.1", prepared.endpoint.substringAfter(':').toInt())
                sockets.add(socket)
                socket.soTimeout = 10_000
                socket.send(
                    0,
                    ClientHandshakePacket.SERIALIZER,
                    ClientHandshakePacket(775, "localhost", 25565, ClientHandshakePacket.Intent.LOGIN),
                )
                socket.send(
                    0,
                    ClientLoginStartPacket.SERIALIZER,
                    ClientLoginStartPacket(request.identity.username, UUID.fromString(request.identity.uuid)),
                )
                val challenge = socket.packet(ConnectionState.LOGIN) as LoginPluginRequestPacket
                val setup =
                    PlayerSetup
                        .newBuilder()
                        .setOperationId(
                            request.operationId,
                        ).setCapability(prepared.capability)
                        .build()
                socket.send(
                    2,
                    ClientLoginPluginResponsePacket.SERIALIZER,
                    ClientLoginPluginResponsePacket(challenge.messageId(), setup.toByteArray()),
                )
                check(socket.packet(ConnectionState.LOGIN) is LoginSuccessPacket) { "Admission rejected" }
                socket.send(3, ClientLoginAcknowledgedPacket.SERIALIZER, ClientLoginAcknowledgedPacket())
                socket.send(0, ClientSettingsPacket.SERIALIZER, ClientSettingsPacket(ClientSettings.DEFAULT))
                while (true) {
                    when (socket.packet(ConnectionState.CONFIGURATION)) {
                        is SelectKnownPacksPacket -> {
                            socket.send(
                                7,
                                ClientSelectKnownPacksPacket.SERIALIZER,
                                ClientSelectKnownPacksPacket(emptyList()),
                            )
                        }

                        is FinishConfigurationPacket -> {
                            break
                        }

                        else -> {}
                    }
                }
                socket.send(3, ClientFinishConfigurationPacket.SERIALIZER, ClientFinishConfigurationPacket())
                return socket
            }

            fun arrive(
                socket: Socket,
                operation: String,
            ) {
                val readerFailure = CompletableFuture<Unit>()
                Thread.startVirtualThread {
                    try {
                        while (true) {
                            val packet = socket.packet(ConnectionState.PLAY)
                            if (packet is PlayerPositionAndLookPacket) {
                                socket.send(
                                    0,
                                    ClientTeleportConfirmPacket.SERIALIZER,
                                    ClientTeleportConfirmPacket(packet.teleportId()),
                                )
                            }
                        }
                    } catch (error: Exception) {
                        readerFailure.completeExceptionally(error)
                    }
                }
                val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(10)
                while (service.deliveries().single { it.delivery.operationId == operation }.phase !=
                    DeliveryPhase.DELIVERY_PHASE_ARRIVED
                ) {
                    if (readerFailure.isDone) readerFailure.join()
                    check(System.nanoTime() < deadline) {
                        "Player never arrived: ${MinecraftServer.getConnectionManager().onlinePlayers.map {
                            Triple(
                                it.lastSentTeleportId,
                                it.lastReceivedTeleportId,
                                (it as ManagedPlayer).initialization?.isDone,
                            )
                        }}"
                    }
                    Thread.sleep(10)
                }
            }
            val first = delivery("a", 1)
            val firstSocket = connect(first)
            arrive(firstSocket, first.operationId)
            val oldPlayer = MinecraftServer.getConnectionManager().onlinePlayers.single()
            val destination = delivery("b", 2)
            assertThrows(IllegalStateException::class.java) { connect(destination) }
            val withdrawal =
                PlayerWithdrawal
                    .newBuilder()
                    .setOperationId(
                        first.operationId,
                    ).setOwnerGeneration(1)
                    .build()
            assertEquals(withdrawal, stub().withdrawPlayer(withdrawal))
            assertEquals(withdrawal, stub().withdrawPlayer(withdrawal))
            assertTrue(oldPlayer.isRemoved)
            assertTrue(oldPlayer in closedPlayers)
            assertTrue(MinecraftServer.getConnectionManager().onlinePlayers.isEmpty())
            val next = delivery("b", 3)
            val nextSocket = connect(next)
            arrive(nextSocket, next.operationId)
            manager.finish(command("a")).get(3, TimeUnit.SECONDS)
            val current = MinecraftServer.getConnectionManager().onlinePlayers.single()
            assertEquals(uuid, current.uuid.toString())
            assertTrue(
                manager
                    .get("b", 1)
                    .scope.instances
                    .contains(current.instance),
            )
            assertTrue(current.isOnline)
            assertEquals(setOf(oldPlayer), closedPlayers)
            stub().withdrawPlayer(
                PlayerWithdrawal
                    .newBuilder()
                    .setOperationId(next.operationId)
                    .setOwnerGeneration(3)
                    .build(),
            )
            manager.create(command("c").toBuilder().setSessionType("gated").build()).get(3, TimeUnit.SECONDS)
            val pending = delivery("c", 4)
            connect(pending)
            joinStarted.get(3, TimeUnit.SECONDS)
            assertTrue(
                service.deliveries().none {
                    it.delivery.operationId == pending.operationId &&
                        it.phase == DeliveryPhase.DELIVERY_PHASE_ARRIVED
                },
            )
            val pendingWithdrawal =
                PlayerWithdrawal
                    .newBuilder()
                    .setOperationId(
                        pending.operationId,
                    ).setOwnerGeneration(4)
                    .build()
            val withdrawn = CompletableFuture.supplyAsync { stub().withdrawPlayer(pendingWithdrawal) }
            val conflicting = delivery("c", 5)
            assertThrows(IllegalStateException::class.java) { connect(conflicting) }
            assertTrue(!withdrawn.isDone, "Withdrawal must await the old asynchronous join")
            joinFinished.complete(null)
            assertEquals(pendingWithdrawal, withdrawn.get(3, TimeUnit.SECONDS))
            val replacement = delivery("c", 6)
            val replacementSocket = connect(replacement)
            arrive(replacementSocket, replacement.operationId)
            stub().withdrawPlayer(
                PlayerWithdrawal
                    .newBuilder()
                    .setOperationId(replacement.operationId)
                    .setOwnerGeneration(6)
                    .build(),
            )
            manager.finish(command("c")).get(3, TimeUnit.SECONDS)
            manager.finish(command("b")).get(3, TimeUnit.SECONDS)
        } finally {
            sockets.forEach { it.close() }
            service.close()
            channel.shutdownNow().awaitTermination(3, TimeUnit.SECONDS)
            server.shutdownNow().awaitTermination(3, TimeUnit.SECONDS)
            MinecraftServer.process().stop()
        }
    }
}
