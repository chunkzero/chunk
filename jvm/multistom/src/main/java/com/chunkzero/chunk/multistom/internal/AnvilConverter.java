package com.chunkzero.chunk.multistom.internal;

import net.hollowcube.polar.PolarChunk;
import net.hollowcube.polar.PolarDataConverter;
import net.hollowcube.polar.PolarSection;
import net.hollowcube.polar.PolarWorld;
import net.hollowcube.polar.PolarWriter;
import net.kyori.adventure.nbt.BinaryTagIO;
import net.minestom.server.MinecraftConstants;
import net.minestom.server.ServerProcess;
import net.minestom.server.instance.Chunk;
import net.minestom.server.instance.InstanceContainer;
import net.minestom.server.instance.anvil.AnvilLoader;
import net.minestom.server.instance.block.Block;
import net.minestom.server.instance.light.LightCompute;
import net.minestom.server.registry.DynamicRegistry;
import net.minestom.server.world.DimensionType;
import net.minestom.server.world.biome.Biome;

import org.jetbrains.annotations.ApiStatus;
import org.jetbrains.annotations.Nullable;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Comparator;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.UUID;
import java.util.function.IntFunction;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/**
 * Converts an Anvil save's overworld to a Polar world for the Gradle plugin, with this runtime's
 * Minestom and Polar. The same save converts to the same bytes.
 */
@ApiStatus.Internal
public final class AnvilConverter {
    private static final Pattern REGION = Pattern.compile("r\\.(-?\\d+)\\.(-?\\d+)\\.mca");
    private static final int MIN_SECTION = -4;
    private static final int MAX_SECTION = 19;
    // PolarWriter reads this many heightmap slots; the runtime computes heightmaps itself.
    private static final int HEIGHTMAPS = 32;
    private static final PolarDataConverter VERSION =
            new PolarDataConverter() {
                @Override
                public int dataVersion() {
                    return MinecraftConstants.DATA_VERSION;
                }
            };

    private AnvilConverter() {}

    /** {@code <save> <output.polar> [<fromX> <fromZ> <toX> <toZ>]}, inclusive chunk coordinates. */
    public static void main(String[] args) throws IOException {
        if (args.length != 2 && args.length != 6)
            throw new IllegalArgumentException(
                    "Usage: AnvilConverter <save> <output> [<fromX> <fromZ> <toX> <toZ>]");
        var range =
                args.length == 6
                        ? Arrays.stream(args, 2, 6).mapToInt(Integer::parseInt).toArray()
                        : null;
        byte[] polar;
        try {
            polar = convert(Path.of(args[0]), range);
        } catch (IllegalArgumentException error) {
            System.err.println(error.getMessage());
            System.exit(1);
            return;
        }
        var output = Path.of(args[1]).toAbsolutePath();
        Files.createDirectories(output.getParent());
        Files.write(output, polar);
    }

    /**
     * Converts the full chunks of {@code save}, within {@code range} ({@code fromX, fromZ, toX,
     * toZ}) if given.
     *
     * @throws IllegalArgumentException if the save is unreadable, from another Minecraft version or
     *     has no chunks to convert
     */
    static byte[] convert(Path save, int @Nullable [] range) throws IOException {
        checkVersion(save.resolve("level.dat"), save);
        var regions = save.resolve("dimensions/minecraft/overworld/region");
        if (!Files.isDirectory(regions)) {
            throw new IllegalArgumentException(
                    Files.isDirectory(save.resolve("region"))
                            ? save + " was saved before Minecraft 26.1. " + upgrade()
                            : save + " has no dimensions/minecraft/overworld/region directory");
        }
        var chunks = new ArrayList<PolarChunk>();
        try (var process = ServerProcess.create()) {
            process.exceptionManager()
                    .setExceptionHandler(
                            error -> {
                                throw new IllegalArgumentException(error.getMessage(), error);
                            });
            var instance =
                    new InstanceContainer(process, UUID.randomUUID(), DimensionType.OVERWORLD);
            var loader = new AnvilLoader(save, DimensionType.OVERWORLD.key());
            for (var region : regions(regions)) {
                for (int z = 0; z < 32; z++) {
                    for (int x = 0; x < 32; x++) {
                        int chunkX = region[0] * 32 + x, chunkZ = region[1] * 32 + z;
                        if (range != null && !inside(range, chunkX, chunkZ)) continue;
                        Chunk chunk;
                        try {
                            chunk = loader.loadChunk(instance, chunkX, chunkZ);
                        } catch (IllegalArgumentException error) {
                            throw new IllegalArgumentException(
                                    "Could not read chunk %d, %d of %s: %s"
                                            .formatted(chunkX, chunkZ, save, error.getMessage()),
                                    error);
                        }
                        if (chunk == null) continue;
                        var data = chunk.tagHandler().asCompound();
                        var status = data.getString("status");
                        if (status.isEmpty() || status.equals("minecraft:full")) {
                            check(
                                    data.getInt("DataVersion"),
                                    "Chunk %d, %d of %s".formatted(chunkX, chunkZ, save));
                            chunk.lockReadLock();
                            try {
                                chunks.add(polar(process.registries().biome(), chunk));
                            } finally {
                                chunk.unlockReadLock();
                            }
                        }
                        loader.unloadChunk(chunk);
                    }
                }
            }
        }
        if (chunks.isEmpty())
            throw new IllegalArgumentException(
                    save
                            + " has no generated chunks"
                            + (range == null ? "" : " in its chunk range"));
        var world =
                new PolarWorld(
                        PolarWorld.LATEST_VERSION,
                        MinecraftConstants.DATA_VERSION,
                        PolarWorld.CompressionType.ZSTD,
                        (byte) MIN_SECTION,
                        (byte) MAX_SECTION,
                        new byte[0],
                        chunks);
        return PolarWriter.write(world, VERSION);
    }

    private static void checkVersion(Path level, Path save) throws IOException {
        if (!Files.isRegularFile(level)) return;
        try (var input = Files.newInputStream(level)) {
            var data =
                    BinaryTagIO.unlimitedReader()
                            .read(input, BinaryTagIO.Compression.GZIP)
                            .getCompound("Data");
            check(data.getInt("DataVersion"), save.toString());
        }
    }

    private static void check(int version, String subject) {
        if (version != MinecraftConstants.DATA_VERSION)
            throw new IllegalArgumentException(
                    "%s has data version %d, but the runtime targets Minecraft %s (data version %d). %s"
                            .formatted(
                                    subject,
                                    version,
                                    MinecraftConstants.VERSION_NAME,
                                    MinecraftConstants.DATA_VERSION,
                                    upgrade()));
    }

    private static String upgrade() {
        return "Open it in Minecraft "
                + MinecraftConstants.VERSION_NAME
                + ", save it and build again; Optimize World upgrades every chunk.";
    }

    /** Region coordinates of the {@code r.<x>.<z>.mca} files, in order. */
    private static List<int[]> regions(Path directory) throws IOException {
        try (var files = Files.list(directory)) {
            return files.map(file -> REGION.matcher(file.getFileName().toString()))
                    .filter(Matcher::matches)
                    .map(
                            match ->
                                    new int[] {
                                        Integer.parseInt(match.group(1)),
                                        Integer.parseInt(match.group(2))
                                    })
                    .sorted(
                            Comparator.<int[]>comparingInt(region -> region[0])
                                    .thenComparingInt(region -> region[1]))
                    .toList();
        }
    }

    private static boolean inside(int[] range, int x, int z) {
        return x >= Math.min(range[0], range[2])
                && x <= Math.max(range[0], range[2])
                && z >= Math.min(range[1], range[3])
                && z <= Math.max(range[1], range[3]);
    }

    private static PolarChunk polar(DynamicRegistry<Biome> biomes, Chunk chunk) {
        var sections = new PolarSection[MAX_SECTION - MIN_SECTION + 1];
        var blockEntities = new ArrayList<PolarChunk.BlockEntity>();
        for (int i = 0; i < sections.length; i++) {
            int sectionY = MIN_SECTION + i;
            var section = chunk.getSection(sectionY);
            var blocks = new Palette(id -> Block.fromStateId(id).state());
            int[] blockData = null;
            if (section.blockPalette().count() == 0) {
                blocks.index(Block.AIR.stateId());
            } else {
                var data = new int[PolarSection.BLOCK_PALETTE_SIZE];
                section.blockPalette()
                        .getAll(
                                (x, y, z, state) ->
                                        data[y * 256 + z * 16 + x] = blocks.index(state));
                blockData = data;
                for (int y = 0; y < Chunk.CHUNK_SECTION_SIZE; y++) {
                    for (int z = 0; z < Chunk.CHUNK_SIZE_Z; z++) {
                        for (int x = 0; x < Chunk.CHUNK_SIZE_X; x++) {
                            int blockY = sectionY * Chunk.CHUNK_SECTION_SIZE + y;
                            var block = chunk.getBlock(x, blockY, z, Block.Getter.Condition.CACHED);
                            if (block == null || block.handler() == null && !block.hasNbt())
                                continue;
                            var handler = block.handler();
                            blockEntities.add(
                                    new PolarChunk.BlockEntity(
                                            x,
                                            blockY,
                                            z,
                                            handler == null ? null : handler.getKey().asString(),
                                            block.nbt()));
                        }
                    }
                }
            }
            var biomeNames = new Palette(id -> biomes.getKey(id).key().asString());
            var biomeData = new int[PolarSection.BIOME_PALETTE_SIZE];
            section.biomePalette()
                    .getAll((x, y, z, id) -> biomeData[y * 16 + z * 4 + x] = biomeNames.index(id));
            var blockLight = section.blockLight().array();
            var skyLight = section.skyLight().array();
            sections[i] =
                    new PolarSection(
                            blocks.names(),
                            blockData,
                            biomeNames.names(),
                            biomeData,
                            light(blockLight),
                            blockLight,
                            light(skyLight),
                            skyLight);
        }
        return new PolarChunk(
                chunk.getChunkX(),
                chunk.getChunkZ(),
                sections,
                blockEntities,
                new int[HEIGHTMAPS][],
                new byte[0]);
    }

    private static PolarSection.LightContent light(byte[] data) {
        if (data.length == 0) return PolarSection.LightContent.MISSING;
        if (Arrays.equals(data, LightCompute.EMPTY_CONTENT)) return PolarSection.LightContent.EMPTY;
        if (Arrays.equals(data, LightCompute.CONTENT_FULLY_LIT))
            return PolarSection.LightContent.FULL;
        return PolarSection.LightContent.PRESENT;
    }

    /** A section palette in first-seen order. */
    private static final class Palette {
        private final IntFunction<String> name;
        private final Map<Integer, Integer> indices = new HashMap<>();
        private final List<String> names = new ArrayList<>();

        Palette(IntFunction<String> name) {
            this.name = name;
        }

        int index(int id) {
            return indices.computeIfAbsent(
                    id,
                    ignored -> {
                        names.add(name.apply(id));
                        return names.size() - 1;
                    });
        }

        String[] names() {
            return names.toArray(String[]::new);
        }
    }
}
