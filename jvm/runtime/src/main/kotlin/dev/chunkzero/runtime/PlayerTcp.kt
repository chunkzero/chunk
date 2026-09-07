package dev.chunkzero.runtime

import chunk.v1.GameplayOuterClass.PlayerSetup
import java.io.InputStream
import java.io.OutputStream
import java.net.InetSocketAddress
import java.net.ServerSocket
import java.net.Socket
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Semaphore

/** Bounded loopback acceptor; a socket is permanently assigned to one delivery. */
internal class PlayerTcp(
    private val attach: (PlayerSetup, Socket) -> Unit,
) : AutoCloseable {
    private val listener = ServerSocket().apply { bind(InetSocketAddress("127.0.0.1", 0), 128) }
    private val slots = Semaphore(128)
    private val sockets = ConcurrentHashMap.newKeySet<Socket>()
    val endpoint = "127.0.0.1:${listener.localPort}"

    init {
        Thread.startVirtualThread {
            while (!listener.isClosed) {
                val socket =
                    try {
                        listener.accept()
                    } catch (_: Exception) {
                        break
                    }
                if (!slots.tryAcquire()) {
                    socket.close()
                    continue
                }
                sockets.add(socket)
                Thread.startVirtualThread {
                    try {
                        socket.use {
                            it.tcpNoDelay = true
                            it.soTimeout = 5000
                            val setup = PlayerSetup.parseFrom(readFrame(it.getInputStream(), 4096))
                            attach(setup, it)
                        }
                    } catch (_: Exception) {
                        // A failed setup or stream closes only this delivery.
                    } finally {
                        sockets.remove(socket)
                        slots.release()
                    }
                }
            }
        }
    }

    override fun close() {
        listener.close()
        sockets.toList().forEach { it.close() }
    }

    companion object {
        fun readFrame(
            input: InputStream,
            limit: Int,
        ): ByteArray {
            var length = 0
            for (index in 0..2) {
                val next = input.read()
                check(next >= 0) { "Truncated frame" }
                length = length or ((next and 127) shl (index * 7))
                if (next and 128 == 0) {
                    require(length in 1..limit) { "Invalid frame size" }
                    val bytes = input.readNBytes(length)
                    check(bytes.size == length) { "Truncated frame" }
                    return bytes
                }
            }
            error("Invalid frame length")
        }

        fun writeFrame(
            output: OutputStream,
            bytes: ByteArray,
        ) {
            require(bytes.size in 1..FrameConnection.MAX_PACKET_BYTES)
            var length = bytes.size
            while (length >= 128) {
                output.write((length and 127) or 128)
                length = length ushr 7
            }
            output.write(length)
            output.write(bytes)
        }
    }
}
