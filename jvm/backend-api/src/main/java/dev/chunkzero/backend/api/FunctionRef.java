package dev.chunkzero.backend.api;

public sealed interface FunctionRef<A, R> permits QueryRef, MutationRef, ActionRef {
    String path();

    JsonType<A> arguments();

    JsonType<R> result();
}
