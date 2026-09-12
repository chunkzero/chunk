package dev.chunkzero.runtime.minestom.internal;

import dev.chunkzero.runtime.SessionProvider;
import dev.chunkzero.runtime.SessionRegistration;
import dev.chunkzero.runtime.SessionType;

import org.jetbrains.annotations.ApiStatus;

import java.util.Map;
import java.util.ServiceConfigurationError;
import java.util.ServiceLoader;
import java.util.TreeMap;

/** Resolves local session factories and binds their backend caller identity. */
@ApiStatus.Internal
public final class AppRegistry {
    private AppRegistry() {}

    public static Map<String, SessionRegistration> load(String app, ClassLoader loader) {
        var factories = new TreeMap<String, SessionRegistration>();
        try {
            for (var provider :
                    ServiceLoader.load(SessionProvider.class, loader).stream().toList()) {
                var declaration = provider.type().getAnnotation(SessionType.class);
                if (declaration == null
                        || !declaration.value().matches("[A-Za-z_][A-Za-z0-9_]{0,127}"))
                    throw new IllegalArgumentException(
                            "Session provider requires a valid @SessionType");
                var key = app + "/" + declaration.value();
                if (factories.put(key, new SessionRegistration(app, () -> provider.get().create()))
                        != null)
                    throw new IllegalArgumentException("Duplicate session type: " + key);
            }
        } catch (ServiceConfigurationError error) {
            throw new IllegalArgumentException("Invalid session provider", error);
        }
        if (factories.isEmpty() || factories.size() > 128)
            throw new IllegalArgumentException("App requires 1–128 session factories");
        return Map.copyOf(factories);
    }
}
