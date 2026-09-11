package dev.chunkzero.runtime.bootstrap;

import dev.chunkzero.backend.api.BackendJson;
import dev.chunkzero.runtime.SessionProvider;
import dev.chunkzero.runtime.SessionRegistration;

import org.jetbrains.annotations.ApiStatus;

import java.io.IOException;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.Collections;
import java.util.HexFormat;
import java.util.Map;
import java.util.TreeMap;

/** Reads the single app contract emitted by the build plugin. */
@ApiStatus.Internal
public final class AppRegistry {
    private static final String MANIFEST = "META-INF/chunk/app.json";

    private AppRegistry() {}

    public record Loaded(
            AppManifest manifest, String digest, Map<String, SessionRegistration> factories) {}

    public static Loaded read(ClassLoader loader) throws IOException {
        var resources = Collections.list(loader.getResources(MANIFEST));
        if (resources.size() != 1)
            throw new IllegalArgumentException("Executable requires exactly one app manifest");
        byte[] bytes;
        try (var input = resources.getFirst().openStream()) {
            bytes = input.readNBytes(65_537);
            if (bytes.length > 65_536) throw new IllegalArgumentException("App manifest too large");
        }
        var manifest = BackendJson.mapper().readValue(bytes, AppManifest.class);
        var factories = new TreeMap<String, SessionRegistration>();
        for (var entry : manifest.sessions().entrySet()) {
            try {
                var type =
                        Class.forName(entry.getValue().provider(), false, loader)
                                .asSubclass(SessionProvider.class);
                var constructor = type.getConstructor();
                factories.put(
                        manifest.id() + "/" + entry.getKey(),
                        new SessionRegistration(
                                manifest.id(),
                                () -> {
                                    try {
                                        return constructor.newInstance().create();
                                    } catch (ReflectiveOperationException error) {
                                        throw new IllegalStateException(
                                                "Cannot create session provider", error);
                                    }
                                }));
            } catch (ReflectiveOperationException | ClassCastException error) {
                throw new IllegalArgumentException(
                        "Invalid session provider: " + entry.getValue().provider(), error);
            }
        }
        try {
            return new Loaded(
                    manifest,
                    HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(bytes)),
                    Map.copyOf(factories));
        } catch (NoSuchAlgorithmException error) {
            throw new AssertionError(error);
        }
    }

    public static Map<String, SessionRegistration> load(ClassLoader loader) throws IOException {
        return read(loader).factories();
    }
}
