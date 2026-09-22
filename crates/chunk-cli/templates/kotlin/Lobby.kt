package example

import dev.chunkzero.generated.BackendTypes.Shared.Greetings.MessageArgs
import dev.chunkzero.generated.CoroutineBackendClient
import dev.chunkzero.runtime.ChunkMinestom
import dev.chunkzero.runtime.ChunkProcess
import dev.chunkzero.runtime.CoroutineSession
import dev.chunkzero.runtime.Session
import dev.chunkzero.runtime.SessionProvider
import dev.chunkzero.runtime.SessionScope
import dev.chunkzero.runtime.SessionType
import dev.chunkzero.runtime.coroutines
import net.kyori.adventure.text.Component
import net.minestom.server.ServerProcess
import net.minestom.server.entity.Player
import net.minestom.server.instance.block.Block

@SessionType("default")
class Lobby : SessionProvider {
    override fun create(): Session = GreetingSession()
}

private class GreetingSession : CoroutineSession() {
    private lateinit var scope: SessionScope

    override suspend fun create(scope: SessionScope) {
        this.scope = scope
        scope.createInstance().setGenerator { it.modifier().fillHeight(0, 40, Block.GRASS_BLOCK) }
    }

    override suspend fun join(player: Player) {
        val backend = CoroutineBackendClient(scope.coroutines.backend(requireNotNull(scope.backend), player))
        val result = backend.shared.greetings.message(MessageArgs(player.username))
        player.sendMessage(Component.text(result.message()))
    }
}

fun main() {
    ChunkProcess.connect().use { chunk ->
        ChunkMinestom.attach(chunk, ServerProcess.create()).use { minestom ->
            minestom.start()
            chunk.ready()
            chunk.awaitShutdown()
        }
    }
}
