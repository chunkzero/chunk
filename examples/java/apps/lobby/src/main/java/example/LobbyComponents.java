package example;

import dev.chunkzero.backend.client.BackendSession;
import dev.chunkzero.generated.BackendClient;
import dev.chunkzero.runtime.Component;

public final class LobbyComponents {
    private LobbyComponents() {}

    @Component(Component.Scope.SESSION)
    public static BackendClient backend(BackendSession session) {
        return new BackendClient(session);
    }
}
