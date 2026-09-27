package dev.chunkzero.backend.client;

import dev.chunkzero.backend.api.JsonType;

/** A typed slot in a group, including its immutable encoded arguments and session binding. */
public final class BoundQuery<T> {
    final BackendSession owner;
    final Transport.Invocation request;
    final JsonType<T> type;

    BoundQuery(BackendSession owner, Transport.Invocation request, JsonType<T> type) {
        this.owner = owner;
        this.request = request;
        this.type = type;
    }
}
