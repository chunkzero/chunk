package com.chunkzero.chunk.backend.api;

public sealed interface FunctionRef<A, R> permits QueryRef, MutationRef, ActionRef {
    String path();

    JsonType<A> arguments();

    JsonType<R> result();
}
