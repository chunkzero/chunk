package dev.chunkzero.backend.api;

import java.util.Objects;

/** A generated wire contract; caller authority and session lifetime are supplied by the runtime. */
public record SessionMethodRef<A, R>(
        String app, String session, String name, JsonType<A> arguments, JsonType<R> result) {
    public SessionMethodRef {
        Objects.requireNonNull(app);
        Objects.requireNonNull(session);
        Objects.requireNonNull(name);
        Objects.requireNonNull(arguments);
        Objects.requireNonNull(result);
    }
}
