package example.lobby;

import dev.chunkzero.runtime.ChunkMinestom;
import dev.chunkzero.runtime.ChunkProcess;
import dev.chunkzero.runtime.SessionProvider;
import dev.chunkzero.runtime.SessionType;

import net.minestom.server.ServerProcess;

@SessionType("default")
public final class Lobby implements SessionProvider {
    public static void main(String[] args) throws Exception {
        try (var chunk = ChunkProcess.connect();
                var minestom = ChunkMinestom.attach(chunk, ServerProcess.create())) {
            minestom.start();
            chunk.ready();
            chunk.awaitShutdown();
        }
    }

    @Override
    public LobbySession create() {
        return new LobbySession();
    }
}
