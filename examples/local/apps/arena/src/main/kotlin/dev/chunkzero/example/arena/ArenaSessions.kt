package dev.chunkzero.example.arena

import dev.chunkzero.example.ExampleSessions
import dev.chunkzero.runtime.ChunkMinestom
import dev.chunkzero.runtime.ChunkProcess
import dev.chunkzero.runtime.SessionProvider
import dev.chunkzero.runtime.SessionType
import net.minestom.server.MinecraftServer

@SessionType("default")
class ArenaSessions : SessionProvider {
    override fun create() = ExampleSessions.arena()
}

@SessionType(value = "large", machineProfile = "large", capacity = 32)
class LargeArenaSessions : SessionProvider {
    override fun create() = ExampleSessions.arena()
}

fun main() {
    ChunkProcess.connect().use { chunk ->
        ChunkMinestom.attach(chunk, MinecraftServer.init()).use { minestom ->
            minestom.start()
            chunk.ready()
            chunk.awaitShutdown()
        }
    }
}
