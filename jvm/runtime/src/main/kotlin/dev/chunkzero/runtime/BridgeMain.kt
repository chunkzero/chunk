package dev.chunkzero.runtime

import chunk.v1.Common.DeploymentRef
import io.grpc.netty.shaded.io.grpc.netty.NettyServerBuilder
import net.minestom.server.MinecraftServer
import net.minestom.server.instance.LightingChunk
import net.minestom.server.instance.block.Block
import net.minestom.server.timer.TaskSchedule
import java.net.InetSocketAddress
import java.util.concurrent.TimeUnit

fun main() {
    val environment = RuntimeEnvironment.load()
    val authentication = ProcessAuthentication(environment.processToken)
    val deployment =
        DeploymentRef
            .newBuilder()
            .setEnvironment(environment.environment)
            .setDeployment(environment.deployment)
            .build()
    val minecraft = MinecraftServer.init()
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
            .maxInboundMessageSize(65_536)
            .intercept(authentication)
            .addService(gameplay)
            .build()
    try {
        minecraft.start("127.0.0.1", 0)
        gameplay.endpoint = "127.0.0.1:${process.server().port}"
        server.start()
    } catch (error: Exception) {
        server.shutdownNow()
        process.stop()
        gameplay.close()
        throw error
    }
    MinecraftServer
        .getSchedulerManager()
        .buildTask { gameplay.flush() }
        .repeat(TaskSchedule.tick(1))
        .schedule()
    Runtime.getRuntime().addShutdownHook(
        Thread {
            server.shutdownNow().awaitTermination(5, TimeUnit.SECONDS)
            process.stop()
            gameplay.close()
        },
    )
    println("Gameplay bridge ready on 127.0.0.1:25566; Minecraft listener at ${gameplay.endpoint}")
    server.awaitTermination()
}
