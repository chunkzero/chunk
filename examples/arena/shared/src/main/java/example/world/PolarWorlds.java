package example.world;

import dev.chunkzero.runtime.SessionScope;

import net.hollowcube.polar.PolarReader;
import net.hollowcube.polar.PolarWorld;
import net.minestom.server.instance.InstanceContainer;
import net.minestom.server.instance.LightingChunk;
import net.minestom.server.world.DimensionType;

import java.io.IOException;
import java.io.UncheckedIOException;
import java.util.concurrent.CompletableFuture;

/** Reads Polar worlds from the classpath and loads them into session instances. */
public final class PolarWorlds {
    private PolarWorlds() {}

    /** Parses a world shipped in the app's resources, such as {@code /worlds/lobby.polar}. */
    public static PolarWorld read(String resource) {
        try (var input = PolarWorlds.class.getResourceAsStream(resource)) {
            if (input == null) throw new IllegalArgumentException("Missing world " + resource);
            return PolarReader.read(input.readAllBytes());
        } catch (IOException error) {
            throw new UncheckedIOException(error);
        }
    }

    /**
     * Creates an instance owned by {@code scope} with every chunk of {@code world} loaded and lit.
     */
    public static CompletableFuture<InstanceContainer> load(SessionScope scope, PolarWorld world) {
        var instance =
                scope.createInstance(
                        DimensionType.OVERWORLD, new PolarLoader(scope.getProcess(), world));
        instance.setChunkSupplier(LightingChunk::new);
        var chunks =
                world.chunks().stream()
                        .map(chunk -> instance.loadChunk(chunk.x(), chunk.z()))
                        .toArray(CompletableFuture<?>[]::new);
        return CompletableFuture.allOf(chunks).thenApply(ignored -> instance);
    }
}
