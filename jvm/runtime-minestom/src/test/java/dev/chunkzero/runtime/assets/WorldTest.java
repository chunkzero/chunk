package dev.chunkzero.runtime.assets;

import static org.junit.jupiter.api.Assertions.*;

import dev.chunkzero.backend.api.WorldAsset;
import dev.chunkzero.runtime.TestScopes;

import net.hollowcube.polar.PolarWorld;
import net.hollowcube.polar.PolarWriter;
import net.minestom.server.ServerProcess;
import net.minestom.server.instance.LightingChunk;
import net.minestom.server.instance.block.Block;

import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.concurrent.TimeUnit;

class WorldTest {
    @TempDir Path directory;

    @Test
    void copiesAreSeparateAndSharedViewsCantChangeTheWorld() throws Exception {
        var file =
                Files.write(directory.resolve("koth.polar"), PolarWriter.write(new PolarWorld()));
        var world = new World(new WorldAsset("arena", "koth"), file);
        try (var process = ServerProcess.create()) {
            var first = TestScopes.create(process);
            var second = TestScopes.create(process);

            var copy = world.copy(first).get(5, TimeUnit.SECONDS);
            var other = world.copy(second).get(5, TimeUnit.SECONDS);
            copy.setBlock(0, 64, 0, Block.STONE);
            assertEquals(Block.STONE, copy.getBlock(0, 64, 0));
            other.loadChunk(0, 0).join();
            assertEquals(Block.AIR, other.getBlock(0, 64, 0));

            var view = world.shared(first).get(5, TimeUnit.SECONDS);
            assertSame(
                    view.getInstanceContainer(),
                    world.shared(second).get(5, TimeUnit.SECONDS).getInstanceContainer());
            var container = view.getInstanceContainer();
            assertThrows(
                    UnsupportedOperationException.class,
                    () -> view.setBlock(0, 64, 0, Block.STONE));
            assertThrows(
                    UnsupportedOperationException.class,
                    () -> container.setBlock(0, 64, 0, Block.STONE));
            var chunk = container.loadChunk(0, 0).join();
            chunk.lockWriteLock();
            try {
                assertThrows(
                        UnsupportedOperationException.class,
                        () -> chunk.setBlock(0, 64, 0, Block.STONE));
            } finally {
                chunk.unlockWriteLock();
            }
            assertThrows(
                    UnsupportedOperationException.class,
                    () -> container.generateChunk(0, 0, unit -> {}));
            assertThrows(UnsupportedOperationException.class, () -> container.unloadChunk(chunk));
            assertThrows(
                    UnsupportedOperationException.class,
                    () -> container.setChunkSupplier(LightingChunk::new));
            assertEquals(Block.AIR, container.getBlock(0, 64, 0));
        }
    }
}
