package dev.chunkzero.runtime.assets;

import dev.chunkzero.backend.api.WorldAsset;

import org.jetbrains.annotations.Nullable;

import java.nio.file.Path;

/**
 * The files and worlds of the deployment this JVM runs, from the read-only directory the platform
 * names in {@code CHUNK_ASSETS}.
 */
public final class Assets {
    private static @Nullable AssetDirectory directory;

    private Assets() {}

    /**
     * Resolves {@code path}, relative to an {@code assets/} directory with forward slashes: the
     * app's own file, or else the project's shared one.
     *
     * @throws IllegalArgumentException if the path leaves {@code assets/} or neither has the file
     */
    public static Path file(String path) {
        return directory().file(path);
    }

    /**
     * Resolves one of this app's worlds, such as a generated {@code Worlds.Arena.KOTH}. Each world
     * is resolved once per JVM, so its parsed data and shared instance are reused.
     *
     * @throws IllegalArgumentException if the world belongs to another app or isn't deployed
     */
    public static World world(WorldAsset asset) {
        return directory().world(asset);
    }

    private static synchronized AssetDirectory directory() {
        if (directory == null)
            directory =
                    new AssetDirectory(Path.of(required("CHUNK_ASSETS")), required("CHUNK_APP_ID"));
        return directory;
    }

    private static String required(String name) {
        var value = System.getenv(name);
        if (value == null || value.isBlank())
            throw new IllegalArgumentException(name + " is required");
        return value;
    }
}
