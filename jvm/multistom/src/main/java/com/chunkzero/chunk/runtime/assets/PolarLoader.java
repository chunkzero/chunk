// Adapted from Hollow Cube Polar 1.16.0 (MIT); see META-INF/POLAR-LICENSE.
// Loading only, bound to a process's registries; codec and file layout unchanged.
package com.chunkzero.chunk.runtime.assets;

import static net.minestom.server.instance.Chunk.CHUNK_SECTION_SIZE;

import net.hollowcube.polar.PolarChunk;
import net.hollowcube.polar.PolarSection;
import net.hollowcube.polar.PolarWorld;
import net.minestom.server.ServerProcess;
import net.minestom.server.instance.Chunk;
import net.minestom.server.instance.ChunkLoader;
import net.minestom.server.instance.Instance;
import net.minestom.server.instance.Section;
import net.minestom.server.instance.block.Block;
import net.minestom.server.instance.light.LightCompute;
import net.minestom.server.registry.RegistryKey;
import net.minestom.server.world.biome.Biome;

import org.jetbrains.annotations.Nullable;

import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;

/** Loads chunks from a parsed Polar world. Its worlds are read-only, so nothing is saved. */
final class PolarLoader implements ChunkLoader {
    private static final System.Logger LOG = System.getLogger(PolarLoader.class.getName());

    private final ServerProcess process;
    private final PolarWorld world;
    private final Map<String, Integer> biomes = new ConcurrentHashMap<>();
    private final int plains;

    PolarLoader(ServerProcess process, PolarWorld world) {
        this.process = process;
        this.world = world;
        plains = process.registries().biome().getId(Biome.PLAINS);
    }

    @Override
    public @Nullable Chunk loadChunk(Instance instance, int chunkX, int chunkZ) {
        var data = world.chunkAt(chunkX, chunkZ);
        if (data == null) return null;
        var chunk = instance.getChunkSupplier().createChunk(instance, chunkX, chunkZ);
        chunk.lockWriteLock();
        try {
            var sections = data.sections();
            for (int i = 0; i < sections.length; i++) {
                int sectionY = world.minSection() + i;
                if (sections[i].isEmpty()
                        || sectionY < chunk.getMinSection()
                        || sectionY >= chunk.getMaxSection()) continue;
                loadSection(sections[i], chunk.getSection(sectionY));
            }
            for (var blockEntity : data.blockEntities()) loadBlockEntity(chunk, blockEntity);
        } finally {
            chunk.unlockWriteLock();
        }
        return chunk;
    }

    @Override
    public void saveChunk(Chunk chunk) {}

    private void loadSection(PolarSection data, Section section) {
        var names = data.blockPalette();
        var states = new int[names.length];
        for (int i = 0; i < names.length; i++) {
            var block = Block.fromState(names[i]);
            if (block == null) {
                LOG.log(
                        System.Logger.Level.ERROR,
                        "Unknown block state {0}, loaded as air",
                        names[i]);
                block = Block.AIR;
            }
            states[i] = block.stateId();
        }
        if (states.length == 1) {
            section.blockPalette().fill(states[0]);
        } else {
            var indices = data.blockData();
            for (int y = 0; y < CHUNK_SECTION_SIZE; y++) {
                for (int z = 0; z < CHUNK_SECTION_SIZE; z++) {
                    for (int x = 0; x < CHUNK_SECTION_SIZE; x++) {
                        int index =
                                y * CHUNK_SECTION_SIZE * CHUNK_SECTION_SIZE
                                        + z * CHUNK_SECTION_SIZE
                                        + x;
                        section.blockPalette().set(x, y, z, states[indices[index]]);
                    }
                }
            }
        }

        var biomeNames = data.biomePalette();
        var biomeIds = new int[biomeNames.length];
        for (int i = 0; i < biomeNames.length; i++) {
            biomeIds[i] = biomes.computeIfAbsent(biomeNames[i], this::biome);
        }
        if (biomeIds.length == 1) {
            section.biomePalette().fill(biomeIds[0]);
        } else {
            var indices = data.biomeData();
            for (int y = 0; y < CHUNK_SECTION_SIZE / 4; y++) {
                for (int z = 0; z < CHUNK_SECTION_SIZE / 4; z++) {
                    for (int x = 0; x < CHUNK_SECTION_SIZE / 4; x++) {
                        int index = indices[x + z * 4 + y * 16];
                        section.biomePalette()
                                .set(x, y, z, index < biomeIds.length ? biomeIds[index] : plains);
                    }
                }
            }
        }

        var blockLight = light(data.blockLightContent(), data.blockLight());
        if (blockLight != null) section.setBlockLight(blockLight);
        var skyLight = light(data.skyLightContent(), data.skyLight());
        if (skyLight != null) section.setSkyLight(skyLight);
    }

    private int biome(String name) {
        var id = process.registries().biome().getId(RegistryKey.unsafeOf(name));
        if (id != -1) return id;
        LOG.log(System.Logger.Level.ERROR, "Unknown biome {0}, loaded as plains", name);
        return plains;
    }

    private static byte @Nullable [] light(PolarSection.LightContent content, byte[] data) {
        return switch (content) {
            case MISSING -> null;
            case EMPTY -> LightCompute.EMPTY_CONTENT;
            case FULL -> LightCompute.CONTENT_FULLY_LIT;
            case PRESENT -> data;
        };
    }

    private void loadBlockEntity(Chunk chunk, PolarChunk.BlockEntity entity) {
        var block = chunk.getBlock(entity.x(), entity.y(), entity.z(), Block.Getter.Condition.TYPE);
        if (entity.id() != null)
            block = block.withHandler(process.blockManager().getHandlerOrDummy(entity.id()));
        if (entity.data() != null) block = block.withNbt(entity.data());
        chunk.setBlock(entity.x(), entity.y(), entity.z(), block);
    }
}
