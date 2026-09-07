package dev.chunkzero.runtime

import chunk.v1.Supervision.ProcessRegistration
import chunk.v1.SupervisorGrpc
import io.grpc.Metadata
import io.grpc.netty.shaded.io.grpc.netty.NettyChannelBuilder
import io.grpc.stub.MetadataUtils
import java.net.URI
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

/** Reattachment repeats the frozen registration; it never disposes gameplay. */
internal class Registration(
    endpoint: String,
    token: String,
    registration: ProcessRegistration,
) : AutoCloseable {
    private val address = URI(endpoint).also { require(it.host == "127.0.0.1" && it.port > 0 && it.scheme == "http") }
    private val channel = NettyChannelBuilder.forAddress(address.host, address.port).usePlaintext().build()
    private val closed = AtomicBoolean()
    private val worker =
        Thread.startVirtualThread {
            val metadata =
                Metadata().apply {
                    put(Metadata.Key.of("authorization", Metadata.ASCII_STRING_MARSHALLER), "Bearer $token")
                }
            val client =
                SupervisorGrpc
                    .newBlockingStub(channel)
                    .withInterceptors(MetadataUtils.newAttachHeadersInterceptor(metadata))
            while (!closed.get()) {
                try {
                    check(
                        client.withDeadlineAfter(3, TimeUnit.SECONDS).registerProcess(registration) ==
                            registration.identity,
                    )
                } catch (_: Exception) {
                    // Existing authenticated TCP deliveries retain their original ownership.
                }
                try {
                    Thread.sleep(1000)
                } catch (_: InterruptedException) {
                    break
                }
            }
        }

    override fun close() {
        closed.set(true)
        channel.shutdownNow()
        worker.interrupt()
    }
}
