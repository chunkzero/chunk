// Adapted from Hollow Cube Polar 1.16.0 (MIT); see META-INF/POLAR-LICENSE.
// Scoped Minestom API bindings, public light setters, and loading only; codec and file layout
// unchanged.
package example.world;

import static net.minestom.server.instance.Chunk.CHUNK_SECTION_SIZE;

import net.hollowcube.polar.PolarChunk;
import net.hollowcube.polar.PolarSection;
import net.hollowcube.polar.PolarSection.LightContent;
import net.hollowcube.polar.PolarWorld;
import net.hollowcube.polar.PolarWorldAccess;
import net.minestom.server.ServerProcess;
import net.minestom.server.command.builder.arguments.minecraft.ArgumentBlockState;
import net.minestom.server.command.builder.exception.ArgumentSyntaxException;
import net.minestom.server.instance.*;
import net.minestom.server.instance.block.Block;
import net.minestom.server.instance.light.LightCompute;
import net.minestom.server.network.NetworkBuffer;
import net.minestom.server.world.biome.Biome;

import org.jetbrains.annotations.Contract;
import org.jetbrains.annotations.NotNull;
import org.jetbrains.annotations.Nullable;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

import java.util.*;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.locks.ReentrantReadWriteLock;

@SuppressWarnings({"UnstableApiUsage", "deprecation"})
public class PolarLoader implements ChunkLoader {
    static final Logger logger = LoggerFactory.getLogger(PolarLoader.class);
    private final ServerProcess process;

    private final Map<String, Integer> biomeReadCache = new ConcurrentHashMap<>();

    private final ReentrantReadWriteLock worldDataLock = new ReentrantReadWriteLock();
    private final PolarWorld worldData;

    private PolarWorldAccess worldAccess;
    private boolean parallel = false;
    private boolean loadLighting = true;

    private int plainsBiomeId = 0; // Always 0 in minestom

    public PolarLoader(ServerProcess process, PolarWorld worldData) {
        this.process = process;
        this.worldData = worldData;
        this.worldAccess =
                new PolarWorldAccess() {
                    public int getBiomeId(String name) {
                        return process.registries()
                                .biome()
                                .getId(net.minestom.server.registry.RegistryKey.unsafeOf(name));
                    }

                    public String getBiomeName(int id) {
                        var key = process.registries().biome().getKey(id);
                        if (key == null) throw new IllegalArgumentException("Unknown biome " + id);
                        return key.key().asString();
                    }
                };
    }

    public @NotNull PolarWorld world() {
        return worldData;
    }

    @Contract("_ -> this")
    public @NotNull PolarLoader setWorldAccess(@NotNull PolarWorldAccess worldAccess) {
        this.worldAccess = worldAccess;

        this.plainsBiomeId = this.worldAccess.getBiomeId(Biome.PLAINS.key().asString());
        if (this.plainsBiomeId == -1) {
            throw new IllegalStateException("Plains biome not found");
        }

        return this;
    }

    /**
     * Sets the loader to save and load in parallel. <br>
     * <br>
     * The Polar loader on its own supports parallel load out of the box, but a user implementation
     * of {@link PolarWorldAccess} may not support parallel operations, so care must be taken when
     * enabling this option.
     *
     * @param parallel True to load and save chunks in parallel, false otherwise.
     * @return this
     */
    @Contract("_ -> this")
    public @NotNull PolarLoader setParallel(boolean parallel) {
        this.parallel = parallel;
        return this;
    }

    @Contract("_ -> this")
    public @NotNull PolarLoader setLoadLighting(boolean loadLighting) {
        this.loadLighting = loadLighting;
        return this;
    }

    // Loading

    @Override
    public boolean supportsParallelLoading() {
        return parallel;
    }

    @Override
    public void loadInstance(@NotNull Instance instance) {
        var userData = worldData.userData();
        if (userData.length > 0) {
            worldAccess.loadWorldData(instance, NetworkBuffer.wrap(userData, 0, userData.length));
        }
    }

    @Override
    public @Nullable Chunk loadChunk(@NotNull Instance instance, int chunkX, int chunkZ) {
        // Only need to lock for this tiny part, chunks are immutable.
        worldDataLock.readLock().lock();
        var chunkData = worldData.chunkAt(chunkX, chunkZ);
        worldDataLock.readLock().unlock();
        if (chunkData == null) return null;

        // We are making the assumption here that the chunk height is the same as this world.
        // Polar includes world height metadata in the prelude and assumes all chunks match
        // those values. We check that the dimension settings match in #loadInstance, so
        // here it can be ignored/assumed.

        // Load the chunk
        var chunk = instance.getChunkSupplier().createChunk(instance, chunkX, chunkZ);
        chunk.lockWriteLock();
        try {
            int sectionY = chunk.getMinSection();
            for (var sectionData : chunkData.sections()) {
                if (sectionData.isEmpty()) {
                    sectionY++;
                    continue;
                }

                var section = chunk.getSection(sectionY);
                loadSection(sectionData, section);
                sectionY++;
            }

            for (var blockEntity : chunkData.blockEntities()) {
                loadBlockEntity(chunk, blockEntity);
            }

            worldAccess.loadHeightmaps(chunk, chunkData.heightmaps());

            var userData = chunkData.userData();
            if (userData.length > 0) {
                worldAccess.loadChunkData(chunk, NetworkBuffer.wrap(userData, 0, userData.length));
            }
        } finally {
            chunk.unlockWriteLock();
        }

        return chunk;
    }

    private void loadSection(@NotNull PolarSection sectionData, @NotNull Section section) {
        // assumed that section is _not_ empty

        // Blocks
        var rawBlockPalette = sectionData.blockPalette();
        var blockPalette = new Block[rawBlockPalette.length];
        for (int i = 0; i < rawBlockPalette.length; i++) {
            try {
                //noinspection deprecation
                blockPalette[i] = ArgumentBlockState.staticParse(rawBlockPalette[i]);
            } catch (ArgumentSyntaxException e) {
                logger.error(
                        "Failed to parse block state: {} ({})", rawBlockPalette[i], e.getMessage());
                blockPalette[i] = Block.AIR;
            }
        }
        if (blockPalette.length == 1) {
            section.blockPalette().fill(blockPalette[0].stateId());
        } else {
            final var paletteData = sectionData.blockData();
            for (int y = 0; y < CHUNK_SECTION_SIZE; y++) {
                for (int z = 0; z < CHUNK_SECTION_SIZE; z++) {
                    for (int x = 0; x < CHUNK_SECTION_SIZE; x++) {
                        int index =
                                y * CHUNK_SECTION_SIZE * CHUNK_SECTION_SIZE
                                        + z * CHUNK_SECTION_SIZE
                                        + x;
                        section.blockPalette()
                                .set(x, y, z, blockPalette[paletteData[index]].stateId());
                    }
                }
            }
        }

        // Biomes
        var rawBiomePalette = sectionData.biomePalette();
        var biomePalette = new int[rawBiomePalette.length];
        for (int i = 0; i < rawBiomePalette.length; i++) {
            biomePalette[i] =
                    biomeReadCache.computeIfAbsent(
                            rawBiomePalette[i],
                            name -> {
                                var biomeId = this.worldAccess.getBiomeId(name);
                                if (biomeId == -1) {
                                    logger.error("Failed to find biome: {}", name);
                                    biomeId = plainsBiomeId;
                                }
                                return biomeId;
                            });
        }
        if (biomePalette.length == 1) {
            section.biomePalette().fill(biomePalette[0]);
        } else {
            final var paletteData = sectionData.biomeData();
            for (int y = 0; y < CHUNK_SECTION_SIZE / 4; y++) {
                for (int z = 0; z < CHUNK_SECTION_SIZE / 4; z++) {
                    for (int x = 0; x < CHUNK_SECTION_SIZE / 4; x++) {
                        int index = x + (z) * 4 + (y) * 16;

                        var paletteIndex = paletteData[index];
                        if (paletteIndex >= biomePalette.length) {
                            logger.error(
                                    "Invalid biome palette index. This is probably a corrupted"
                                        + " world, but it has been loaded with plains instead. No"
                                        + " data has been written.");
                            section.biomePalette().set(x, y, z, plainsBiomeId);
                        } else {
                            section.biomePalette().set(x, y, z, biomePalette[paletteIndex]);
                        }
                    }
                }
            }
        }

        // Light
        if (loadLighting && sectionData.blockLightContent() != LightContent.MISSING)
            section.setBlockLight(
                    getLightArray(sectionData.blockLightContent(), sectionData.blockLight()));
        if (loadLighting && sectionData.skyLightContent() != LightContent.MISSING)
            section.setSkyLight(
                    getLightArray(sectionData.skyLightContent(), sectionData.skyLight()));
    }

    static byte[] getLightArray(@NotNull LightContent content, byte @Nullable [] data) {
        return switch (content) {
            case MISSING -> null;
            case EMPTY -> LightCompute.EMPTY_CONTENT;
            case FULL -> LightCompute.CONTENT_FULLY_LIT;
            case PRESENT -> data;
        };
    }

    @NotNull
    Block createBlockEntity(@NotNull Chunk chunk, @NotNull PolarChunk.BlockEntity blockEntity) {
        // Fetch the block type, we can ignore Handler/NBT since we are about to replace it
        var block =
                chunk.getBlock(
                        blockEntity.x(),
                        blockEntity.y(),
                        blockEntity.z(),
                        Block.Getter.Condition.TYPE);
        if (blockEntity.id() != null)
            block = block.withHandler(process.blockManager().getHandlerOrDummy(blockEntity.id()));
        if (blockEntity.data() != null) block = block.withNbt(blockEntity.data());
        return block;
    }

    void loadBlockEntity(@NotNull Chunk chunk, @NotNull PolarChunk.BlockEntity blockEntity) {
        var block = createBlockEntity(chunk, blockEntity);
        chunk.setBlock(blockEntity.x(), blockEntity.y(), blockEntity.z(), block);
    }

    // The example's worlds are read-only, so nothing is saved.
    @Override
    public void saveChunk(@NotNull Chunk chunk) {}
}
