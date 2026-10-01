package dev.chunkzero.backend.api;

import java.util.Objects;

public record ActionRef<A, R>(String path, JsonType<A> arguments, JsonType<R> result)
        implements FunctionRef<A, R> {
    public ActionRef {
        Objects.requireNonNull(path);
        Objects.requireNonNull(arguments);
        Objects.requireNonNull(result);
    }
}
