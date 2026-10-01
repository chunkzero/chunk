package example.arena;

import dev.chunkzero.generated.ArenaSessionProviders;
import dev.chunkzero.generated.SessionConfigs;
import dev.chunkzero.runtime.ChunkMinestom;
import dev.chunkzero.runtime.ChunkProcess;
import dev.chunkzero.runtime.SessionCreation;
import dev.chunkzero.runtime.SessionType;

import net.minestom.server.ServerProcess;

@SessionType("koth")
public final class Arena implements ArenaSessionProviders.Koth {
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
