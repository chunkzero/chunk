package dev.chunkzero.backend.client;

import chunk.v1.BackendOuterClass.BackendCall;
import dev.chunkzero.backend.api.Codec;

/** A typed slot in a group, including its immutable encoded arguments and session binding. */
public final class BoundQuery<T> {
    final BackendSession owner;
    final BackendCall request;
    final Codec<T> codec;
    BoundQuery(BackendSession owner, BackendCall request, Codec<T> codec) {
        this.owner = owner; this.request = request; this.codec = codec;
    }
}
