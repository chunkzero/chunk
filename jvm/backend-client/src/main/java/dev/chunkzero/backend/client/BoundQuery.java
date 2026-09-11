package dev.chunkzero.backend.client;

import chunk.v1.BackendOuterClass.BackendQuery;

import dev.chunkzero.backend.api.JsonType;

/** A typed slot in a group, including its immutable encoded arguments and session binding. */
public final class BoundQuery<T> {
    final BackendSession owner;
    final BackendQuery request;
    final JsonType<T> type;

    BoundQuery(BackendSession owner, BackendQuery request, JsonType<T> type) {
        this.owner = owner;
        this.request = request;
        this.type = type;
    }
}
