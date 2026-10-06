package example;

import com.chunkzero.chunk.generated.BackendClient;
import com.chunkzero.chunk.generated.BackendTypes.Shared.Greetings.MessageArgs;
import com.chunkzero.chunk.generated.BackendTypes.Shared.Greetings.MessageResult;
import com.chunkzero.chunk.multistom.ChunkMinestom;
import com.chunkzero.chunk.multistom.Session;
import com.chunkzero.chunk.multistom.SessionScope;
import com.chunkzero.chunk.runtime.ChunkProcess;
import com.chunkzero.chunk.runtime.SessionProvider;
import com.chunkzero.chunk.runtime.SessionType;

import net.kyori.adventure.text.Component;
import net.minestom.server.ServerProcess;
import net.minestom.server.entity.Player;
import net.minestom.server.instance.block.Block;

import java.util.Objects;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionStage;

@SessionType("default")
public final class Lobby implements SessionProvider<Session> {
    public static void main(String[] args) throws Exception {
        try (var chunk = ChunkProcess.connect();
                var minestom = ChunkMinestom.attach(chunk, ServerProcess.create())) {
            minestom.start();
            chunk.ready();
            chunk.awaitShutdown();
        }
    }

    @Override
    public Session create() {
        return new GreetingSession();
    }

    private static final class GreetingSession extends Session {
        private SessionScope scope;
        private BackendClient backend;

        @Override
        public CompletionStage<Void> onCreate(SessionScope scope) {
            this.scope = scope;
            backend = new BackendClient(Objects.requireNonNull(scope.getBackend()));
            scope.createInstance()
                    .setGenerator(unit -> unit.modifier().fillHeight(0, 40, Block.GRASS_BLOCK));
            return CompletableFuture.completedFuture(null);
        }

        @Override
        public CompletionStage<Void> onJoin(Player player) {
            CompletableFuture<MessageResult> message =
                    backend.shared().greetings().message(new MessageArgs(player.getUsername()));
            return message.thenCompose(
                    result ->
                            scope.onTick(
                                    () -> player.sendMessage(Component.text(result.message()))));
        }
    }
}
