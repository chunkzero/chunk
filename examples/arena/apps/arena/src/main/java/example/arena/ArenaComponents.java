package example.arena;

import dev.chunkzero.backend.client.BackendSession;
import dev.chunkzero.generated.BackendClient;
import dev.chunkzero.runtime.Component;

public final class ArenaComponents {
    private ArenaComponents() {}

    @Component(Component.Scope.SESSION)
    public static BackendClient backend(BackendSession session) {
        return new BackendClient(session);
    }
}
