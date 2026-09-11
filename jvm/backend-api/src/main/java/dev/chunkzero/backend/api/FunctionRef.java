package dev.chunkzero.backend.api;

public sealed interface FunctionRef<A, R> permits QueryRef, MutationRef {
    String path();

    Codec<A> arguments();

    Codec<R> result();
}
