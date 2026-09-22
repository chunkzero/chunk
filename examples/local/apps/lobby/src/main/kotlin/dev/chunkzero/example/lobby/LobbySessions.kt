package dev.chunkzero.example.lobby

import dev.chunkzero.example.ExampleSessions
import dev.chunkzero.runtime.ChunkMinestom
import dev.chunkzero.runtime.ChunkProcess
import dev.chunkzero.runtime.SessionProvider
import dev.chunkzero.runtime.SessionType
import net.minestom.server.ServerProcess

@SessionType("default")
class LobbySessions : SessionProvider {
    override fun create() = ExampleSessions.lobby()
}

fun main() {
    ChunkProcess.connect().use { chunk ->
        val server = ServerProcess.create()
        ExampleSessions.register(server)
        ChunkMinestom.attach(chunk, server).use { minestom ->
            minestom.start()
            chunk.ready()
            chunk.awaitShutdown()
        }
    }
}
