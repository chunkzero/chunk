package com.chunkzero.chunk.multistom.internal;

import static org.junit.jupiter.api.Assertions.*;

import net.hollowcube.polar.PolarReader;
import net.kyori.adventure.nbt.BinaryTagIO;
import net.kyori.adventure.nbt.CompoundBinaryTag;
import net.minestom.server.MinecraftConstants;
import net.minestom.server.ServerProcess;
import net.minestom.server.instance.DynamicChunk;
import net.minestom.server.instance.InstanceContainer;
import net.minestom.server.instance.anvil.AnvilLoader;
import net.minestom.server.instance.block.Block;
import net.minestom.server.world.DimensionType;

import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.Map;
import java.util.UUID;

class AnvilConverterTest {
    private static final Block STAIRS = Block.OAK_STAIRS.withProperty("facing", "east");

    @TempDir Path save;

    @Test
    void aSaveConvertsToTheSameBytesEveryTimeWithinItsChunkRange() throws IOException {
        try (var process = ServerProcess.create()) {
            var instance =
                    new InstanceContainer(process, UUID.randomUUID(), DimensionType.OVERWORLD);
            var loader = new AnvilLoader(save, DimensionType.OVERWORLD.key());
            for (var position : new int[][] {{0, 0}, {1, 0}, {-1, -1}}) {
                var chunk = new DynamicChunk(instance, position[0], position[1]);
                chunk.lockWriteLock();
                chunk.setBlock(1, 64, 1, STAIRS);
                chunk.setBlock(2, 70, 2, Block.STONE);
                chunk.unlockWriteLock();
                loader.saveChunk(chunk);
                loader.unloadChunk(chunk);
            }
        }
        var polar = AnvilConverter.convert(save, null);
        assertArrayEquals(polar, AnvilConverter.convert(save, null));
        var world = PolarReader.read(polar);
        assertEquals(3, world.chunks().size());
        assertEquals(MinecraftConstants.DATA_VERSION, world.dataVersion());
        var section = world.chunkAt(-1, -1).sections()[64 / 16 + 4];
        assertTrue(List.of(section.blockPalette()).contains(STAIRS.state()));
        var cropped = PolarReader.read(AnvilConverter.convert(save, new int[] {0, 0, 1, 0}));
        assertEquals(2, cropped.chunks().size());
        assertNull(cropped.chunkAt(-1, -1));
    }

    @Test
    void savesOfAnotherMinecraftVersionAreRejected() throws IOException {
        var data = CompoundBinaryTag.builder().putInt("DataVersion", 4189).build();
        try (var output = Files.newOutputStream(save.resolve("level.dat"))) {
            BinaryTagIO.writer()
                    .writeNamed(
                            Map.entry("", CompoundBinaryTag.builder().put("Data", data).build()),
                            output,
                            BinaryTagIO.Compression.GZIP);
        }
        var error =
                assertThrows(
                        IllegalArgumentException.class, () -> AnvilConverter.convert(save, null));
        assertTrue(
                error.getMessage()
                        .contains("Open it in Minecraft " + MinecraftConstants.VERSION_NAME),
                error.getMessage());
    }
}
