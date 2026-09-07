package dev.chunkzero.runtime

import chunk.v1.Common.DeploymentRef
import io.grpc.Metadata
import io.grpc.ServerCall
import io.grpc.ServerCallHandler
import io.grpc.ServerInterceptor
import io.grpc.Status
import io.grpc.netty.shaded.io.grpc.netty.NettyServerBuilder
import net.minestom.server.MinecraftServer
import net.minestom.server.instance.LightingChunk
import net.minestom.server.instance.block.Block
import java.net.InetSocketAddress
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit

fun main() {
    val token = requireNotNull(System.getenv("CHUNK_PROCESS_TOKEN")) { "CHUNK_PROCESS_TOKEN is required" }
    require(token.length >= 32) { "Process token must contain at least 32 characters" }
    val deployment =
        DeploymentRef
            .newBuilder()
            .setEnvironment(System.getenv("CHUNK_ENVIRONMENT") ?: "local")
            .setDeployment(System.getenv("CHUNK_DEPLOYMENT") ?: "local")
            .build()
    MinecraftServer.init()
    MinecraftServer.setCompressionThreshold(0)
    val process = MinecraftServer.process()
    val instance = process.instance().createInstanceContainer()
    instance.setChunkSupplier(::LightingChunk)
    instance.setGenerator { it.modifier().fillHeight(0, 40, Block.GRASS_BLOCK) }
    val gameplay = GameplayService(deployment, 1, instance)
    val server =
        NettyServerBuilder
            .forAddress(InetSocketAddress("127.0.0.1", 25566))
            .maxConcurrentCallsPerConnection(128)
            .maxInboundMessageSize(FrameConnection.MAX_PACKET_BYTES + 4096)
            .intercept(ProcessAuthentication(token))
            .addService(gameplay)
            .build()
            .start()
    process.dispatcher().start()
    val ticker = Executors.newSingleThreadScheduledExecutor()
    ticker.scheduleAtFixedRate({
        process.ticker().tick(System.nanoTime())
        gameplay.flush()
    }, 0, 50, TimeUnit.MILLISECONDS)
    Runtime.getRuntime().addShutdownHook(
        Thread {
            gameplay.close()
            server.shutdownNow().awaitTermination(5, TimeUnit.SECONDS)
            ticker.shutdownNow()
            process.stop()
        },
    )
    println("Gameplay bridge ready on 127.0.0.1:25566; no Minecraft listener")
    server.awaitTermination()
}

internal class ProcessAuthentication(
    private val token: String,
) : ServerInterceptor {
    override fun <ReqT : Any, RespT : Any> interceptCall(
        call: ServerCall<ReqT, RespT>,
        headers: Metadata,
        next: ServerCallHandler<ReqT, RespT>,
    ): ServerCall.Listener<ReqT> {
        if (headers.get(Metadata.Key.of("authorization", Metadata.ASCII_STRING_MARSHALLER)) != "Bearer $token") {
            call.close(Status.UNAUTHENTICATED, Metadata())
            return object : ServerCall.Listener<ReqT>() {}
        }
        return next.startCall(call, headers)
    }
}
