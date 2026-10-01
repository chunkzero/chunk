package example.arena;

import dev.chunkzero.backend.client.BackendSession;
import dev.chunkzero.generated.BackendClient;
import dev.chunkzero.runtime.Component;

import example.world.PolarWorlds;

import net.hollowcube.polar.PolarWorld;

public final class ArenaComponents {
    private ArenaComponents() {}

    /** Parsed once per JVM and shared by its arena sessions, which each load their own copy. */
    @Component(Component.Scope.PROCESS)
    public static PolarWorld world() {
        return PolarWorlds.read("/worlds/arena.polar");
    }

    @Component(Component.Scope.SESSION)
    public static BackendClient backend(BackendSession session) {
        return new BackendClient(session);
    }
}
