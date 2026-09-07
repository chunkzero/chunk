package dev.chunkzero.runtime

import net.minestom.server.network.ConnectionState
import net.minestom.server.network.NetworkBuffer
import net.minestom.server.network.packet.PacketWriting
import net.minestom.server.network.packet.server.SendablePacket

internal fun encodeConfiguration(packet: SendablePacket): ByteArray {
    val actual = requireNotNull(SendablePacket.extractServerPacket(ConnectionState.CONFIGURATION, packet))
    val buffer = PacketWriting.allocateTrimmedPacket(ConnectionState.CONFIGURATION, actual, 0)
    val length = buffer.read(NetworkBuffer.VAR_INT)
    require(length in 1..2_097_151)
    return buffer.read(NetworkBuffer.RAW_BYTES)
}
