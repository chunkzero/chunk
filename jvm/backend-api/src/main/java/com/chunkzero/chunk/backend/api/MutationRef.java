package com.chunkzero.chunk.backend.api;

import java.util.Objects;

public record MutationRef<A, R>(String path, JsonType<A> arguments, JsonType<R> result)
        implements FunctionRef<A, R> {
    public MutationRef {
        Objects.requireNonNull(path);
        Objects.requireNonNull(arguments);
        Objects.requireNonNull(result);
    }
}
