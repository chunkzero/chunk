package com.chunkzero.chunk.example.arena

import com.chunkzero.chunk.example.ExampleSessions
import com.chunkzero.chunk.example.generated.ArenaSessionProviders
import com.chunkzero.chunk.example.generated.SessionConfigs
import com.chunkzero.chunk.multistom.ChunkMinestom
import com.chunkzero.chunk.multistom.SessionCreation
import com.chunkzero.chunk.runtime.ChunkProcess
import com.chunkzero.chunk.runtime.SessionType
import net.minestom.server.ServerProcess

@SessionType("default")
class ArenaSessions : ArenaSessionProviders.Default {
    override fun create(creation: SessionCreation<SessionConfigs.Arena.Default.Config>) =
        ExampleSessions.arena("${creation.config().label()} (${creation.maxPlayers()} slots)")
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
