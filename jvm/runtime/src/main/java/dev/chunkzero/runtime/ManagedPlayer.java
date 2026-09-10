package dev.chunkzero.runtime;

import chunk.v1.GameplayOuterClass.PlayerDelivery;
import java.util.Objects;
import java.util.concurrent.CompletableFuture;
import net.minestom.server.entity.Player;
import net.minestom.server.network.player.GameProfile;
import net.minestom.server.network.player.PlayerConnection;
import org.jetbrains.annotations.Nullable;

final class ManagedPlayer extends Player {
    private @Nullable PlayerDelivery binding;
    private @Nullable CompletableFuture<Void> initialization;

    ManagedPlayer(PlayerConnection connection, GameProfile profile) {
        super(connection, profile);
    }

    PlayerDelivery getBinding() {
        if (binding == null) throw new IllegalStateException("Player delivery has not been bound");
        return binding;
    }

    void setBinding(PlayerDelivery binding) {
        this.binding = Objects.requireNonNull(binding);
    }

    @Nullable CompletableFuture<Void> getInitialization() {
        return initialization;
    }

    @Override
    public CompletableFuture<Void> UNSAFE_init() {
        var completion = new CompletableFuture<Void>();
        initialization = completion;
        try {
            super.UNSAFE_init().whenComplete((ignored, error) -> {
                if (error == null) completion.complete(null);
                else completion.completeExceptionally(error);
            });
        } catch (Exception error) {
            completion.completeExceptionally(error);
        }
        return completion;
    }
}
