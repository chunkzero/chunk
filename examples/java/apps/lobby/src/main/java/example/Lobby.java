package example;

import dev.chunkzero.generated.BackendClient;
import dev.chunkzero.generated.BackendTypes.Shared.Greetings.MessageArgs;
import dev.chunkzero.generated.BackendTypes.Shared.Greetings.MessageResult;
import dev.chunkzero.generated.SessionMethods;
import dev.chunkzero.generated.LobbySessionProviders;
import dev.chunkzero.generated.SessionConfigs;
import dev.chunkzero.runtime.ChunkMinestom;
import dev.chunkzero.runtime.ChunkProcess;
import dev.chunkzero.runtime.Session;
import dev.chunkzero.runtime.SessionCreation;
import dev.chunkzero.runtime.SessionScope;
import dev.chunkzero.runtime.SessionType;

import net.kyori.adventure.text.Component;
import net.minestom.server.MinecraftServer;
import net.minestom.server.entity.Player;
import net.minestom.server.instance.InstanceContainer;
import net.minestom.server.instance.block.Block;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionStage;

@SessionType("default")
public final class Lobby implements LobbySessionProviders.Default {
    public static void main(String[] args) throws Exception {
        try (var chunk = ChunkProcess.connect();
                var minestom = ChunkMinestom.attach(chunk, MinecraftServer.init())) {
            minestom.start();
            chunk.ready();
            chunk.awaitShutdown();
        }
    }

    @Override
    public GreetingSession create(SessionCreation<SessionConfigs.Lobby.Default.Config> creation) {
        return new GreetingSession(creation.config().greeting());
    }

    public static final class GreetingSession extends Session
            implements SessionMethods.Lobby.Default.Announce {
        private InstanceContainer instance;
        private SessionScope scope;
        private BackendClient backend;
        private final String greeting;

        public GreetingSession(String greeting) {
            this.greeting = greeting;
        }

        @Override
        public CompletionStage<Void> onCreate(SessionScope scope) {
            this.scope = scope;
            backend = scope.component(BackendClient.class);
            instance = scope.createInstance();
            instance.setGenerator(unit -> unit.modifier().fillHeight(0, 40, Block.GRASS_BLOCK));
            return CompletableFuture.completedFuture(null);
        }

        @Override
        public Long announce(SessionMethods.Lobby.Default.Announce.Args args) {
            var players = instance.getPlayers();
            players.forEach(player -> player.sendMessage(Component.text(args.message())));
            return (long) players.size();
        }

        @Override
        public CompletionStage<Void> onJoin(Player player) {
            CompletableFuture<MessageResult> message =
                    backend.shared().greetings().message(new MessageArgs(player.getUsername()));
            return message.thenCompose(
                    result ->
                            scope.onTick(
                                    () -> player.sendMessage(Component.text(greeting + " " + result.message()))));
        }
    }
}
