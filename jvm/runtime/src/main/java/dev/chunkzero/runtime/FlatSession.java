package dev.chunkzero.runtime;

import net.minestom.server.instance.LightingChunk;
import net.minestom.server.instance.block.Block;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionStage;

final class FlatSession extends Session {
    @Override
    public CompletionStage<Void> onCreate(SessionScope scope) {
        var instance = scope.createInstance();
        instance.setChunkSupplier(LightingChunk::new);
        instance.setGenerator(unit -> unit.modifier().fillHeight(0, 40, Block.GRASS_BLOCK));
        return CompletableFuture.completedFuture(null);
    }
}
