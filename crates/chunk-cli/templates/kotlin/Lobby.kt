package example

import com.chunkzero.chunk.generated.BackendTypes.Shared.Greetings.MessageArgs
import com.chunkzero.chunk.generated.CoroutineBackendClient
import com.chunkzero.chunk.multistom.ChunkMinestom
import com.chunkzero.chunk.multistom.CoroutineSession
import com.chunkzero.chunk.multistom.Session
import com.chunkzero.chunk.multistom.SessionProvider
import com.chunkzero.chunk.multistom.SessionScope
import com.chunkzero.chunk.multistom.coroutines
import com.chunkzero.chunk.runtime.ChunkProcess
import com.chunkzero.chunk.runtime.SessionType
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
