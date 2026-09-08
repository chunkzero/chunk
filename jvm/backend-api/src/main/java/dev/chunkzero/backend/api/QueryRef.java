package dev.chunkzero.backend.api;

import java.util.Objects;

public record QueryRef<A, R>(String path, Codec<A> arguments, Codec<R> result) implements FunctionRef<A, R> {
    public QueryRef { Objects.requireNonNull(path); Objects.requireNonNull(arguments); Objects.requireNonNull(result); }
}
