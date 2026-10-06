package com.chunkzero.chunk.example.lobby

import com.chunkzero.chunk.example.ExampleSessions
import com.chunkzero.chunk.multistom.ChunkMinestom
import com.chunkzero.chunk.multistom.SessionProvider
import com.chunkzero.chunk.runtime.ChunkProcess
import com.chunkzero.chunk.runtime.SessionType
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
