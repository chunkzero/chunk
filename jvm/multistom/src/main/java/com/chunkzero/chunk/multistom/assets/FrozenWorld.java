package com.chunkzero.chunk.multistom.assets;

import net.minestom.server.ServerProcess;
import net.minestom.server.coordinate.Point;
import net.minestom.server.entity.Player;
import net.minestom.server.instance.Chunk;
import net.minestom.server.instance.ChunkLoader;
import net.minestom.server.instance.Instance;
import net.minestom.server.instance.InstanceContainer;
import net.minestom.server.instance.LightingChunk;
import net.minestom.server.instance.block.Block;
import net.minestom.server.instance.block.BlockFace;
import net.minestom.server.instance.block.BlockHandler;
import net.minestom.server.instance.generator.Generator;
import net.minestom.server.registry.RegistryKey;
import net.minestom.server.utils.chunk.ChunkSupplier;
import net.minestom.server.world.DimensionType;
import net.minestom.server.world.biome.Biome;

import org.jetbrains.annotations.Nullable;

import java.util.UUID;
import java.util.concurrent.CompletableFuture;

/**
 * The instance a shared world loads into. Once {@link #freeze frozen}, Minestom's APIs can't change
 * its blocks or biomes, generate, unload or replace its chunks: players' placements and breaks are
 * refused, and anything else throws.
 */
final class FrozenWorld extends InstanceContainer {
    private volatile boolean frozen;
    private boolean constructed;

    FrozenWorld(ServerProcess process, ChunkLoader loader) {
        super(
                process,
                UUID.randomUUID(),
                DimensionType.OVERWORLD,
                loader,
                DimensionType.OVERWORLD.key());
        super.setChunkSupplier(Frozen::new);
        constructed = true;
    }

    void freeze() {
        frozen = true;
    }

    private void check() {
        if (frozen) throw new UnsupportedOperationException("A shared world is read-only");
    }

    @Override
    public void setBlock(int x, int y, int z, Block block, boolean doBlockUpdates) {
        check();
        super.setBlock(x, y, z, block, doBlockUpdates);
    }

    @Override
    public boolean placeBlock(BlockHandler.Placement placement, boolean doBlockUpdates) {
        return !frozen && super.placeBlock(placement, doBlockUpdates);
    }

    @Override
    public boolean breakBlock(
            Player player, Point blockPosition, BlockFace blockFace, boolean doBlockUpdates) {
        return !frozen && super.breakBlock(player, blockPosition, blockFace, doBlockUpdates);
    }

    @Override
    public void setChunkSupplier(ChunkSupplier chunkSupplier) {
        // InstanceContainer's constructor sets a supplier before this class's own.
        if (constructed)
            throw new UnsupportedOperationException("A shared world's chunks are always read-only");
        super.setChunkSupplier(chunkSupplier);
    }

    @Override
    public void setChunkLoader(ChunkLoader chunkLoader) {
        check();
        super.setChunkLoader(chunkLoader);
    }

    @Override
    public void setGenerator(@Nullable Generator generator) {
        check();
        super.setGenerator(generator);
    }

    @Override
    public CompletableFuture<Void> generateChunk(int chunkX, int chunkZ, Generator generator) {
        check();
        return super.generateChunk(chunkX, chunkZ, generator);
    }

    @Override
    public synchronized void unloadChunk(Chunk chunk) {
        check();
        super.unloadChunk(chunk);
    }

    private static final class Frozen extends LightingChunk {
        private final FrozenWorld world;

        Frozen(Instance instance, int chunkX, int chunkZ) {
            super(instance, chunkX, chunkZ);
            world = (FrozenWorld) instance;
        }

        @Override
        public void setBlock(
                int x,
                int y,
                int z,
                Block block,
                BlockHandler.@Nullable Placement placement,
                BlockHandler.@Nullable Destroy destroy) {
            world.check();
            super.setBlock(x, y, z, block, placement, destroy);
        }

        @Override
        public void setBiome(int x, int y, int z, RegistryKey<Biome> biome) {
            world.check();
            super.setBiome(x, y, z, biome);
        }

        @Override
        public void reset() {
            world.check();
            super.reset();
        }
    }
}
