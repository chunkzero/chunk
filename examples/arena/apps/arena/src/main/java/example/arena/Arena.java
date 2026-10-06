package example.arena;

import com.chunkzero.chunk.generated.ArenaSessionProviders;
import com.chunkzero.chunk.generated.SessionConfigs;
import com.chunkzero.chunk.multistom.ChunkMinestom;
import com.chunkzero.chunk.runtime.ChunkProcess;
import com.chunkzero.chunk.runtime.SessionCreation;
import com.chunkzero.chunk.runtime.SessionType;

import net.minestom.server.ServerProcess;

@SessionType("koth")
public final class Arena implements ArenaSessionProviders.Koth<ArenaSession> {
    public static void main(String[] args) throws Exception {
        try (var chunk = ChunkProcess.connect();
                var minestom = ChunkMinestom.attach(chunk, ServerProcess.create())) {
            minestom.start();
            chunk.ready();
            chunk.awaitShutdown();
        }
    }

    @Override
    public ArenaSession create(SessionCreation<SessionConfigs.Arena.Koth.Config> creation) {
        var config = creation.config();
        return new ArenaSession(
                new Match.Rules(
                        Math.toIntExact(config.targetScore()),
                        Math.toIntExact(config.timeLimitSeconds())));
    }
}
