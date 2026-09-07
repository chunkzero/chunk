package dev.chunkzero.runtime

import chunk.v1.Common.DeploymentRef
import chunk.v1.Supervision.ProcessIdentity
import chunk.v1.Supervision.ProcessRegistration
import io.grpc.netty.shaded.io.grpc.netty.NettyServerBuilder
import net.minestom.server.MinecraftServer
import net.minestom.server.instance.LightingChunk
import net.minestom.server.instance.block.Block
import java.net.InetSocketAddress
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong

fun main() {
    val environment = RuntimeEnvironment.load()
    val token = environment.processToken
    val authentication = ProcessAuthentication(token)
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
    val supervisor = environment.supervisor
    val identity =
        ProcessIdentity
            .newBuilder()
            .setDeployment(deployment)
            .setRuntimeId(environment.runtimeId)
            .setProcessId(environment.processId)
            .setGeneration(environment.processGeneration)
            .setMachineProfile(environment.machineProfile)
            .setArtifactDigest(environment.artifactDigest)
            .build()
    val shutdown = CountDownLatch(1)
    val ticks = AtomicLong()
    val gameplay = GameplayService(deployment, identity.generation, instance, runtimeId = identity.runtimeId)
    val server =
        NettyServerBuilder
            .forAddress(InetSocketAddress("127.0.0.1", if (supervisor == null) 25566 else 0))
            .maxConcurrentCallsPerConnection(128)
            .maxInboundMessageSize(65_536)
            .intercept(authentication)
            .addService(gameplay)
            .addService(ProcessService(identity, gameplay, ticks, shutdown))
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
        .buildTask {
            gameplay.flush()
            ticks.incrementAndGet()
        }.repeat(
            net.minestom.server.timer.TaskSchedule
                .tick(1),
        ).schedule()
    val registration =
        supervisor?.let {
            Registration(
                it,
                token,
                ProcessRegistration
                    .newBuilder()
                    .setIdentity(identity)
                    .setControlEndpoint("127.0.0.1:${server.port}")
                    .setPlayerEndpoint(gameplay.endpoint)
                    .setConfiguration(gameplay.configurationArtifact)
                    .build(),
            )
        }
    val stopped = AtomicBoolean()

    fun close() {
        if (!stopped.compareAndSet(false, true)) return
        registration?.close()
        server.shutdownNow().awaitTermination(5, TimeUnit.SECONDS)
        process.stop()
        gameplay.close()
    }
    Runtime.getRuntime().addShutdownHook(Thread { close() })
    println("Gameplay ready on 127.0.0.1:${server.port}; Minecraft listener at ${gameplay.endpoint}")
    shutdown.await()
    close()
}
