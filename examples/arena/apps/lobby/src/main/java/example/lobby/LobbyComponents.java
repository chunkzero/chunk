package example.lobby;

import dev.chunkzero.backend.client.BackendSession;
import dev.chunkzero.generated.BackendClient;
import dev.chunkzero.runtime.Component;

import example.world.PolarWorlds;

import net.hollowcube.polar.PolarWorld;

public final class LobbyComponents {
    private LobbyComponents() {}

    /** Parsed once per JVM and shared by its lobby sessions, which each load their own copy. */
    @Component(Component.Scope.PROCESS)
    public static PolarWorld world() {
        return PolarWorlds.read("/worlds/lobby.polar");
    }

    @Component(Component.Scope.SESSION)
    public static BackendClient backend(BackendSession session) {
        return new BackendClient(session);
    }
}
