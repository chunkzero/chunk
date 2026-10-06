package com.chunkzero.chunk.runtime;

import com.chunkzero.chunk.backend.api.BackendJson;

import java.util.Map;
import java.util.Objects;
import java.util.ServiceConfigurationError;
import java.util.ServiceLoader;
import java.util.Set;
import java.util.TreeMap;
import java.util.function.Supplier;

/**
 * An app's session providers by qualified session type ({@code app/type}). Creating a session
 * decodes its creation config for {@link ConfiguredSessionProvider}s and requires an empty one for
 * plain providers.
 */
public final class SessionRegistry {
    private static final String NAME = "[A-Za-z_][A-Za-z0-9_]{0,127}";

    @SuppressWarnings("unchecked")
    private static final Class<SessionProvider<?>> SERVICE =
            (Class<SessionProvider<?>>) (Class<?>) SessionProvider.class;

    private final Map<String, Supplier<? extends SessionProvider<?>>> providers;

    /** Registers {@code providers} by qualified session type. */
    public SessionRegistry(Map<String, ? extends SessionProvider<?>> providers) {
        this(copy(providers));
    }

    private SessionRegistry(TreeMap<String, Supplier<? extends SessionProvider<?>>> providers) {
        if (providers.isEmpty() || providers.size() > 128)
            throw new IllegalArgumentException("App requires 1–128 session types");
        this.providers = Map.copyOf(providers);
    }

    /**
     * Loads the {@link SessionProvider} services of {@code app}, each declaring its {@link
     * SessionType}. Every created session gets a fresh provider instance.
     */
    public static SessionRegistry load(String app, ClassLoader loader) {
        if (!app.matches(NAME)) throw new IllegalArgumentException("Invalid app ID: " + app);
        var providers = new TreeMap<String, Supplier<? extends SessionProvider<?>>>();
        try {
            for (var provider : ServiceLoader.load(SERVICE, loader).stream().toList()) {
                var declaration = provider.type().getAnnotation(SessionType.class);
                if (declaration == null || !declaration.value().matches(NAME))
                    throw new IllegalArgumentException(
                            "Session provider requires a valid @SessionType");
                var key = app + "/" + declaration.value();
                if (providers.put(key, provider::get) != null)
                    throw new IllegalArgumentException("Duplicate session type: " + key);
            }
        } catch (ServiceConfigurationError error) {
            throw new IllegalArgumentException("Invalid session provider", error);
        }
        return new SessionRegistry(providers);
    }

    /** The qualified session types. */
    public Set<String> types() {
        return providers.keySet();
    }

    /**
     * Creates a session of {@code type} for {@code capacity} players from its creation config.
     *
     * @throws IllegalArgumentException if the type is unknown or the config is invalid for it
     */
    public Object create(String type, int capacity, String configurationJson) {
        var provider = providers.get(type);
        if (provider == null) throw new IllegalArgumentException("Unknown session type: " + type);
        var instance = provider.get();
        Object session;
        if (instance instanceof ConfiguredSessionProvider<?, ?> configured) {
            session = configured(configured, capacity, configurationJson);
        } else {
            requireEmptyConfiguration(configurationJson);
            session = instance.create();
        }
        return Objects.requireNonNull(session, "Session provider returned null for " + type);
    }

    private static <C> Object configured(
            ConfiguredSessionProvider<C, ?> provider, int capacity, String json) {
        var config = provider.configurationType().read(json);
        return provider.create(new SessionCreation<>(capacity, config));
    }

    private static void requireEmptyConfiguration(String json) {
        var config = BackendJson.mapper().readTree(json);
        if (!config.isObject() || !config.isEmpty())
            throw new IllegalArgumentException(
                    "Session provider does not declare creation configuration");
    }

    private static TreeMap<String, Supplier<? extends SessionProvider<?>>> copy(
            Map<String, ? extends SessionProvider<?>> providers) {
        var copy = new TreeMap<String, Supplier<? extends SessionProvider<?>>>();
        providers.forEach(
                (type, provider) -> {
                    Objects.requireNonNull(provider);
                    copy.put(type, () -> provider);
                });
        return copy;
    }
}
