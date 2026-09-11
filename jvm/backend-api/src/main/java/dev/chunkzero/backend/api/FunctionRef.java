package dev.chunkzero.backend.api;

public sealed interface FunctionRef<A, R> permits QueryRef, MutationRef {
    String path();

    JsonType<A> arguments();

    JsonType<R> result();
}
