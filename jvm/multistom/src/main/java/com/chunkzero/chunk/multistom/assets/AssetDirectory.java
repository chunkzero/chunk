package com.chunkzero.chunk.multistom.assets;

import com.chunkzero.chunk.backend.api.BackendJson;
import com.chunkzero.chunk.backend.api.WorldAsset;

import java.io.IOException;
import java.io.UncheckedIOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;

/**
 * An app's materialized assets: {@code revision.json}, {@code worlds/<name>.polar}, {@code
 * app/<path>} and {@code shared/<path>}.
 */
final class AssetDirectory {
    private final Path root;
    private final String app;
    private final Map<String, World> worlds = new ConcurrentHashMap<>();

    AssetDirectory(Path root, String app) {
        this.root = root;
        this.app = app;
    }

    Path file(String path) {
        var segments = path.split("/", -1);
        for (var segment : segments) {
            if (segment.isEmpty()
                    || segment.equals(".")
                    || segment.equals("..")
                    || segment.contains("\\"))
                throw new IllegalArgumentException(
                        "Asset path " + path + " must be relative and stay inside assets/");
        }
        for (var directory : new String[] {"app", "shared"}) {
            var file = root.resolve(directory).resolve(path);
            if (Files.isRegularFile(file)) return file;
        }
        throw new IllegalArgumentException(
                "No asset file " + path + " in the app's assets/ or the project's assets/");
    }

    World world(WorldAsset asset) {
        if (!asset.app().equals(app))
            throw new IllegalArgumentException(
                    "World "
                            + asset.app()
                            + "/"
                            + asset.name()
                            + " belongs to app "
                            + asset.app()
                            + "; app "
                            + app
                            + " can only load its own worlds");
        return worlds.computeIfAbsent(asset.name(), name -> load(asset));
    }

    private World load(WorldAsset asset) {
        var file = root.resolve("worlds").resolve(asset.name() + ".polar");
        if (!declared(asset.name()) || !Files.isRegularFile(file))
            throw new IllegalArgumentException(
                    "World "
                            + app
                            + "/"
                            + asset.name()
                            + " is not in this deployment's assets; declare it in the app's"
                            + " app.ts and deploy again");
        return new World(asset, file);
    }

    private boolean declared(String world) {
        try {
            var revision =
                    BackendJson.mapper()
                            .readTree(Files.readAllBytes(root.resolve("revision.json")));
            return revision.path("apps").path(app).path("worlds").has(world);
        } catch (IOException error) {
            throw new UncheckedIOException(error);
        }
    }
}
