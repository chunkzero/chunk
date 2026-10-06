package com.chunkzero.chunk.multistom.assets;

import com.chunkzero.chunk.backend.api.WorldAsset;
import com.chunkzero.chunk.multistom.SessionScope;

import net.hollowcube.polar.PolarReader;
import net.hollowcube.polar.PolarWorld;
import net.minestom.server.ServerProcess;
import net.minestom.server.instance.InstanceContainer;
import net.minestom.server.instance.LightingChunk;
import net.minestom.server.instance.SharedInstance;
import net.minestom.server.world.DimensionType;

import org.jetbrains.annotations.Nullable;

import java.io.IOException;
import java.io.UncheckedIOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.UUID;
import java.util.concurrent.CompletableFuture;

/** A Polar world of this app's deployment, as {@link Assets#world} resolves it. */
public final class World {
    private final WorldAsset asset;
    private final Path file;
    private @Nullable PolarWorld polar;
    private @Nullable Shared shared;

    World(WorldAsset asset, Path file) {
        this.asset = asset;
        this.file = file;
    }

    /** Parses the world on first use; later calls return the same world. */
    public synchronized PolarWorld polar() {
        if (polar == null) {
            try {
                polar = PolarReader.read(Files.readAllBytes(file));
            } catch (IOException error) {
                throw new UncheckedIOException("Could not read world " + name(), error);
            }
        }
        return polar;
    }

    /**
     * Creates a fresh instance of this world owned by {@code scope}, completing once every chunk is
     * loaded and lit. Call it on the session's tick thread, like {@link
     * SessionScope#createInstance}.
     */
    public CompletableFuture<InstanceContainer> copy(SessionScope scope) {
        var world = polar();
        var instance =
                scope.createInstance(
                        DimensionType.OVERWORLD, new PolarLoader(scope.getProcess(), world));
        instance.setChunkSupplier(LightingChunk::new);
        return load(instance, world);
    }

    /**
     * Adds a read-only view of this world to {@code scope}, completing once its chunks are loaded.
     * The JVM loads the world into one instance the first time a session asks and keeps it; each
     * session's view goes away with the session. Once loaded, its blocks, biomes and chunks can't
     * be changed through either the view or its instance container: players' placements and breaks
     * are refused, and other changes throw. Call it on the session's tick thread.
     */
    public CompletableFuture<SharedInstance> shared(SessionScope scope) {
        var shared = shared(scope.getProcess());
        var view =
                scope.registerSharedInstance(
                        new SharedInstance(UUID.randomUUID(), shared.container()));
        return shared.loaded().thenApply(ignored -> view);
    }

    private synchronized Shared shared(ServerProcess process) {
        if (shared != null) {
            if (shared.container().process() != process)
                throw new IllegalStateException(name() + " is shared by another server process");
            return shared;
        }
        var world = polar();
        var container = new FrozenWorld(process, new PolarLoader(process, world));
        process.instanceManager().registerInstance(container);
        var loaded = load(container, world).thenRun(container::freeze);
        shared = new Shared(container, loaded);
        return shared;
    }

    private static CompletableFuture<InstanceContainer> load(
            InstanceContainer instance, PolarWorld world) {
        var chunks =
                world.chunks().stream()
                        .map(chunk -> instance.loadChunk(chunk.x(), chunk.z()))
                        .toArray(CompletableFuture<?>[]::new);
        return CompletableFuture.allOf(chunks).thenApply(ignored -> instance);
    }

    private String name() {
        return asset.app() + "/" + asset.name();
    }

    private record Shared(FrozenWorld container, CompletableFuture<?> loaded) {}
}
