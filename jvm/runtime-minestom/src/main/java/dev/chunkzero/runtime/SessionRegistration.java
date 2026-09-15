package dev.chunkzero.runtime;

import dev.chunkzero.backend.api.BackendJson;
import dev.chunkzero.backend.client.BackendSession;

import org.jetbrains.annotations.ApiStatus;
import org.jetbrains.annotations.Nullable;

import java.util.Objects;
import java.util.function.BiFunction;
import java.util.function.Supplier;

/** App identity belongs to registration, independently of the session instance and routing key. */
@ApiStatus.Internal
public final class SessionRegistration {
    private final String appId;
    private final BiFunction<Integer, String, Session> factory;

    public SessionRegistration(String appId, Supplier<Session> factory) {
        this(
                appId,
                (capacity, json) -> {
                    requireEmptyConfiguration(json);
                    return factory.get();
                });
        Objects.requireNonNull(factory);
    }

    private SessionRegistration(String appId, BiFunction<Integer, String, Session> factory) {
        this.appId = Objects.requireNonNull(appId);
        this.factory = Objects.requireNonNull(factory);
        if (!appId.matches("[A-Za-z_][A-Za-z0-9_]{0,127}"))
            throw new IllegalArgumentException("Invalid app ID: " + appId);
    }

    public static SessionRegistration provider(String appId, Supplier<SessionProvider> provider) {
        return new SessionRegistration(
                appId,
                (capacity, json) -> {
                    var instance = provider.get();
                    if (instance instanceof ConfiguredSessionProvider<?> configured)
                        return configured(configured, capacity, json);
                    requireEmptyConfiguration(json);
                    return instance.create();
                });
    }

    Session create() {
        return create(1, "{}");
    }

    Session create(int capacity, String configuration) {
        return Objects.requireNonNull(
                factory.apply(capacity, configuration),
                "Session provider returned null for " + appId);
    }

    private static <C> Session configured(
            ConfiguredSessionProvider<C> provider, int capacity, String json) {
        var config = provider.configurationType().read(json);
        return provider.create(new SessionCreation<>(capacity, config));
    }

    private static void requireEmptyConfiguration(String json) {
        var config = BackendJson.mapper().readTree(json);
        if (!config.isObject() || !config.isEmpty())
            throw new IllegalArgumentException(
                    "Session provider does not declare creation configuration");
    }

    @Nullable
    BackendSession backend(
            String sessionId, @Nullable BiFunction<String, String, BackendSession> clients) {
        return clients == null ? null : clients.apply(sessionId, appId);
    }
}
