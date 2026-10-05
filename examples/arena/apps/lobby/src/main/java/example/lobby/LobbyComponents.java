package example.lobby;

import com.chunkzero.chunk.backend.client.BackendSession;
import com.chunkzero.chunk.generated.BackendClient;
import com.chunkzero.chunk.runtime.Component;

public final class LobbyComponents {
    private LobbyComponents() {}

    @Component(Component.Scope.SESSION)
    public static BackendClient backend(BackendSession session) {
        return new BackendClient(session);
    }
}
