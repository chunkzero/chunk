package dev.chunkzero.runtime.bootstrap;

import dev.chunkzero.runtime.Session;
import dev.chunkzero.runtime.SessionScope;

import net.minestom.server.instance.LightingChunk;
import net.minestom.server.instance.block.Block;

import org.jetbrains.annotations.ApiStatus;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionStage;

@ApiStatus.Internal
public final class FlatSession extends Session {
    @Override
    public CompletionStage<Void> onCreate(SessionScope scope) {
        var instance = scope.createInstance();
        instance.setChunkSupplier(LightingChunk::new);
        instance.setGenerator(unit -> unit.modifier().fillHeight(0, 40, Block.GRASS_BLOCK));
        return CompletableFuture.completedFuture(null);
    }
}
