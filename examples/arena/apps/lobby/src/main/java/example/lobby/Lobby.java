package example.lobby;

import com.chunkzero.chunk.multistom.ChunkMinestom;
import com.chunkzero.chunk.runtime.ChunkProcess;
import com.chunkzero.chunk.runtime.SessionProvider;
import com.chunkzero.chunk.runtime.SessionType;

import net.minestom.server.ServerProcess;

@SessionType("default")
public final class Lobby implements SessionProvider<LobbySession> {
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
