package com.chunkzero.chunk.multistom;

import com.chunkzero.chunk.runtime.Delivery;

import net.minestom.server.entity.Player;
import net.minestom.server.network.player.GameProfile;
import net.minestom.server.network.player.PlayerConnection;

import org.jetbrains.annotations.ApiStatus;
import org.jetbrains.annotations.Nullable;

import java.util.Objects;
import java.util.concurrent.CompletableFuture;

@ApiStatus.Internal
public final class ManagedPlayer extends Player {
    private @Nullable Delivery delivery;
    private @Nullable CompletableFuture<Void> initialization;

    ManagedPlayer(PlayerConnection connection, GameProfile profile) {
        super(connection, profile);
    }

    Delivery getDelivery() {
        if (delivery == null) throw new IllegalStateException("Player delivery has not been bound");
        return delivery;
    }

    public void setDelivery(Delivery delivery) {
        this.delivery = Objects.requireNonNull(delivery);
    }

    @Nullable
    public CompletableFuture<Void> getInitialization() {
        return initialization;
    }

    @Override
    public CompletableFuture<Void> UNSAFE_init() {
        var completion = new CompletableFuture<Void>();
        initialization = completion;
        try {
            super.UNSAFE_init()
                    .whenComplete(
                            (ignored, error) -> {
                                if (error == null) completion.complete(null);
                                else completion.completeExceptionally(error);
                            });
        } catch (Exception error) {
            completion.completeExceptionally(error);
        }
        return completion;
    }
}
