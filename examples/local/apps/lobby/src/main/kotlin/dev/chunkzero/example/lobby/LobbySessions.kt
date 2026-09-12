package dev.chunkzero.example.lobby

import dev.chunkzero.example.ExampleSessions
import dev.chunkzero.runtime.ChunkMinestom
import dev.chunkzero.runtime.ChunkProcess
import dev.chunkzero.runtime.SessionProvider
import dev.chunkzero.runtime.SessionType
import net.minestom.server.MinecraftServer

@SessionType("default")
class LobbySessions : SessionProvider {
    override fun create() = ExampleSessions.lobby()
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
