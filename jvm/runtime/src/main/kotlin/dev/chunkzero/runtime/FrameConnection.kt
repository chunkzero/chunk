package dev.chunkzero.runtime

import net.minestom.server.MinecraftServer
import net.minestom.server.network.ConnectionState
import net.minestom.server.network.NetworkBuffer
import net.minestom.server.network.packet.PacketVanilla
import net.minestom.server.network.packet.PacketWriting
import net.minestom.server.network.packet.server.SendablePacket
import net.minestom.server.network.player.PlayerConnection
import java.net.InetSocketAddress
import java.util.ArrayDeque

/** Plain packet transport. Only the proxy handles client encryption and compression. */
internal class FrameConnection : PlayerConnection() {
    private val outbound = ArrayDeque<ByteArray>()
    private var queuedBytes = 0

    init {
        clientState = ConnectionState.PLAY
        serverState = ConnectionState.PLAY
    }

    override fun getRemoteAddress() = InetSocketAddress("127.0.0.1", 0)

    @Synchronized
    override fun sendPacket(packet: SendablePacket) {
        if (!isOnline) return
        val actual =
            requireNotNull(SendablePacket.extractServerPacket(serverState, packet)) {
                "Preframed socket packets are unsupported by the internal transport"
            }
        val bytes = encode(serverState, actual)
        if (outbound.size >= 256 || queuedBytes + bytes.size > MAX_QUEUED_BYTES) {
            disconnect()
        } else {
            outbound.addLast(bytes)
            queuedBytes += bytes.size
        }
    }

    @Synchronized
    fun poll(): ByteArray? = outbound.pollFirst()?.also { queuedBytes -= it.size }

    @Synchronized
    override fun disconnect() {
        outbound.clear()
        queuedBytes = 0
        super.disconnect()
    }

    fun receive(bytes: ByteArray) {
        require(bytes.isNotEmpty() && bytes.size <= MAX_PACKET_BYTES)
        val buffer = NetworkBuffer.wrap(bytes, 0, bytes.size, MinecraftServer.process())
        val packet = PacketVanilla.CLIENT_PACKET_PARSER.parse(clientState, buffer.read(NetworkBuffer.VAR_INT), buffer)
        require(buffer.readableBytes() == 0L) { "Trailing packet bytes" }
        requireNotNull(player).addPacketToQueue(packet)
    }

    companion object {
        private const val MAX_QUEUED_BYTES = 8 * 1024 * 1024
        const val MAX_PACKET_BYTES = 2_097_151

        fun encode(
            state: ConnectionState,
            packet: SendablePacket,
        ): ByteArray {
            val actual = requireNotNull(SendablePacket.extractServerPacket(state, packet))
            val buffer = PacketWriting.allocateTrimmedPacket(state, actual, 0)
            val length = buffer.read(NetworkBuffer.VAR_INT)
            require(length in 1..MAX_PACKET_BYTES)
            return buffer.read(NetworkBuffer.RAW_BYTES)
        }
    }
}
