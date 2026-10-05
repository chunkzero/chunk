package com.chunkzero.chunk.backend.api;

import java.util.Objects;

public record QueryRef<A, R>(String path, JsonType<A> arguments, JsonType<R> result)
        implements FunctionRef<A, R> {
    public QueryRef {
        Objects.requireNonNull(path);
        Objects.requireNonNull(arguments);
        Objects.requireNonNull(result);
    }
}
