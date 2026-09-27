package dev.chunkzero.runtime

import chunk.sync.v1.Jvm.PlayerSetup
import net.minestom.server.MinecraftConstants
import net.minestom.server.network.ConnectionState
import net.minestom.server.network.NetworkBuffer
import net.minestom.server.network.packet.PacketVanilla
import net.minestom.server.network.packet.client.common.ClientSettingsPacket
import net.minestom.server.network.packet.client.configuration.ClientFinishConfigurationPacket
import net.minestom.server.network.packet.client.configuration.ClientSelectKnownPacksPacket
import net.minestom.server.network.packet.client.handshake.ClientHandshakePacket
import net.minestom.server.network.packet.client.login.ClientLoginAcknowledgedPacket
import net.minestom.server.network.packet.client.login.ClientLoginPluginResponsePacket
import net.minestom.server.network.packet.client.login.ClientLoginStartPacket
import net.minestom.server.network.packet.client.play.ClientTeleportConfirmPacket
import net.minestom.server.network.packet.server.ServerPacket
import net.minestom.server.network.packet.server.configuration.FinishConfigurationPacket
import net.minestom.server.network.packet.server.configuration.SelectKnownPacksPacket
import net.minestom.server.network.packet.server.login.LoginPluginRequestPacket
import net.minestom.server.network.packet.server.play.PlayerPositionAndLookPacket
import net.minestom.server.network.player.ClientSettings
import net.minestom.server.registry.Registries
import org.junit.jupiter.api.Assertions.assertEquals
import java.net.Socket
import java.util.UUID
import java.util.concurrent.CompletableFuture

/** Starts a login as [name] on [port], answering the `chunk:delivery` plugin request with [setup]. */
internal fun login(
    port: Int,
    name: String,
    uuid: UUID,
    setup: PlayerSetup,
): Socket {
    val socket = Socket("127.0.0.1", port)
    socket.soTimeout = 10_000
    socket.send(
        0,
        ClientHandshakePacket.SERIALIZER,
        ClientHandshakePacket(
            MinecraftConstants.PROTOCOL_VERSION,
            "localhost",
            25565,
            ClientHandshakePacket.Intent.LOGIN,
        ),
    )
    socket.send(0, ClientLoginStartPacket.SERIALIZER, ClientLoginStartPacket(name, uuid))
    val challenge = socket.packet(ConnectionState.LOGIN) as LoginPluginRequestPacket
    assertEquals("chunk:delivery", challenge.channel())
    socket.send(
        2,
        ClientLoginPluginResponsePacket.SERIALIZER,
        ClientLoginPluginResponsePacket(challenge.messageId(), setup.toByteArray()),
    )
    return socket
}

/** Finishes configuration after a successful login, entering play. */
internal fun Socket.configure() {
    send(3, ClientLoginAcknowledgedPacket.SERIALIZER, ClientLoginAcknowledgedPacket())
    send(0, ClientSettingsPacket.SERIALIZER, ClientSettingsPacket(ClientSettings.DEFAULT))
    while (true) {
        when (packet(ConnectionState.CONFIGURATION)) {
            is SelectKnownPacksPacket -> {
                send(7, ClientSelectKnownPacksPacket.SERIALIZER, ClientSelectKnownPacksPacket(emptyList()))
            }

            is FinishConfigurationPacket -> {
                break
            }

            else -> {}
        }
    }
    send(3, ClientFinishConfigurationPacket.SERIALIZER, ClientFinishConfigurationPacket())
}

/** Confirms every teleport in the background, so the player arrives; fails once the connection ends. */
internal fun Socket.confirmTeleports(): CompletableFuture<Unit> {
    val ended = CompletableFuture<Unit>()
    Thread.startVirtualThread {
        try {
            while (true) {
                val packet = packet(ConnectionState.PLAY)
                if (packet is PlayerPositionAndLookPacket) {
                    send(0, ClientTeleportConfirmPacket.SERIALIZER, ClientTeleportConfirmPacket(packet.teleportId()))
                }
            }
        } catch (error: Exception) {
            ended.completeExceptionally(error)
        }
    }
    return ended
}

internal fun <T> Socket.send(
    id: Int,
    serializer: NetworkBuffer.Type<T>,
    packet: T,
) {
    val body =
        NetworkBuffer.makeArray { buffer ->
            buffer.write(NetworkBuffer.VAR_INT, id)
            buffer.write(serializer, packet)
        }
    getOutputStream().write(
        NetworkBuffer.makeArray { buffer ->
            buffer.write(NetworkBuffer.VAR_INT, body.size)
            buffer.write(NetworkBuffer.RAW_BYTES, body)
        },
    )
}

internal fun Socket.packet(state: ConnectionState): ServerPacket {
    val input = getInputStream()
    var length = 0
    var shift = 0
    while (true) {
        val next = input.read()
        check(next >= 0 && shift < 21) { "Truncated frame" }
        length = length or ((next and 127) shl shift)
        if (next and 128 == 0) break
        shift += 7
    }
    require(length in 1..2_097_151)
    val bytes = input.readNBytes(length)
    check(bytes.size == length)
    val buffer = NetworkBuffer.wrap(bytes, 0, bytes.size, Registries.vanilla())
    return PacketVanilla.SERVER_PACKET_PARSER.parse(state, buffer.read(NetworkBuffer.VAR_INT), buffer)
}
