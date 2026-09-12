package dev.chunkzero.runtime.minestom.internal;

import dev.chunkzero.runtime.SessionProvider;
import dev.chunkzero.runtime.SessionRegistration;
import dev.chunkzero.runtime.bootstrap.AppManifest;

import org.jetbrains.annotations.ApiStatus;

import java.util.Map;
import java.util.TreeMap;

/** Resolves session factories from the process's validated app contract. */
@ApiStatus.Internal
public final class AppRegistry {
    private AppRegistry() {}

    public static Map<String, SessionRegistration> load(AppManifest manifest, ClassLoader loader) {
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
        return Map.copyOf(factories);
    }
}
