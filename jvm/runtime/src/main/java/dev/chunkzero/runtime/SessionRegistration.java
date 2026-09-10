package dev.chunkzero.runtime;

import dev.chunkzero.backend.client.BackendSession;
import java.util.Objects;
import java.util.function.BiFunction;
import java.util.function.Supplier;
import org.jetbrains.annotations.Nullable;

/** App identity belongs to registration, independently of the session instance and routing key. */
record SessionRegistration(String appId, Supplier<Session> factory) {
    SessionRegistration {
        Objects.requireNonNull(appId);
        Objects.requireNonNull(factory);
        if (!appId.matches("[A-Za-z_][A-Za-z0-9_]{0,127}")) throw new IllegalArgumentException("Invalid app ID: " + appId);
    }

    Session create() {
        return Objects.requireNonNull(factory.get(), "Session provider returned null for " + appId);
    }

    @Nullable BackendSession backend(String sessionId, @Nullable BiFunction<String, String, BackendSession> clients) {
        return clients == null ? null : clients.apply(sessionId, appId);
    }
}
