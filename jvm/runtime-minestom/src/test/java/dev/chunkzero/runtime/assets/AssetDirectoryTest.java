package dev.chunkzero.runtime.assets;

import static org.junit.jupiter.api.Assertions.*;

import dev.chunkzero.backend.api.WorldAsset;

import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;

class AssetDirectoryTest {
    @TempDir Path directory;

    @Test
    void filesResolveFromTheAppBeforeSharedAndNeverLeaveAssets() throws IOException {
        write("app/config/rules.json");
        write("shared/config/rules.json");
        write("shared/lang/en.json");
        var assets = new AssetDirectory(directory, "arena");
        assertEquals(directory.resolve("app/config/rules.json"), assets.file("config/rules.json"));
        assertEquals(directory.resolve("shared/lang/en.json"), assets.file("lang/en.json"));
        for (var path :
                new String[] {
                    "", "/lang/en.json", "../revision.json", "lang/../../revision.json"
                }) {
            assertThrows(IllegalArgumentException.class, () -> assets.file(path), path);
        }
        var missing = assertThrows(IllegalArgumentException.class, () -> assets.file("none.txt"));
        assertTrue(missing.getMessage().contains("none.txt"), missing.getMessage());
    }

    @Test
    void worldsOfOtherAppsOrMissingFromTheDeploymentAreRejected() throws IOException {
        Files.writeString(
                directory.resolve("revision.json"),
                """
                {"version":1,"apps":{"arena":{"worlds":{"koth":{},"gone":{}},"files":{}}}}\
                """);
        write("worlds/koth.polar");
        write("worlds/stale.polar");
        var assets = new AssetDirectory(directory, "arena");
        var koth = assets.world(new WorldAsset("arena", "koth"));
        assertSame(koth, assets.world(new WorldAsset("arena", "koth")));
        var foreign =
                assertThrows(
                        IllegalArgumentException.class,
                        () -> assets.world(new WorldAsset("lobby", "main")));
        assertTrue(foreign.getMessage().contains("belongs to app lobby"), foreign.getMessage());
        for (var name : new String[] {"stale", "gone"}) {
            var missing =
                    assertThrows(
                            IllegalArgumentException.class,
                            () -> assets.world(new WorldAsset("arena", name)));
            assertTrue(missing.getMessage().contains("arena/" + name), missing.getMessage());
        }
    }

    private void write(String path) throws IOException {
        var file = directory.resolve(path);
        Files.createDirectories(file.getParent());
        Files.writeString(file, path);
    }
}
