package example;

import dev.chunkzero.generated.BackendClient;
import dev.chunkzero.generated.BackendTypes.Shared.Greetings.MessageArgs;
import dev.chunkzero.generated.BackendTypes.Shared.Greetings.MessageResult;
import dev.chunkzero.runtime.ChunkMinestom;
import dev.chunkzero.runtime.ChunkProcess;
import dev.chunkzero.runtime.Session;
import dev.chunkzero.runtime.SessionProvider;
import dev.chunkzero.runtime.SessionScope;
import dev.chunkzero.runtime.SessionType;

import net.kyori.adventure.text.Component;
import net.minestom.server.MinecraftServer;
import net.minestom.server.entity.Player;
import net.minestom.server.instance.block.Block;

import java.util.Objects;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionStage;

@SessionType("default")
public final class Lobby implements SessionProvider {
    public static void main(String[] args) throws Exception {
        try (var chunk = ChunkProcess.connect();
                var minestom = ChunkMinestom.attach(chunk, MinecraftServer.init())) {
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
