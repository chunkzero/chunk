package dev.chunkzero.example.arena

import dev.chunkzero.example.ExampleSessions
import dev.chunkzero.example.generated.ArenaSessionProviders
import dev.chunkzero.example.generated.SessionConfigs
import dev.chunkzero.runtime.ChunkMinestom
import dev.chunkzero.runtime.ChunkProcess
import dev.chunkzero.runtime.SessionCreation
import dev.chunkzero.runtime.SessionType
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
