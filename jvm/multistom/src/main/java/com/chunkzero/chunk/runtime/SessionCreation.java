package com.chunkzero.chunk.runtime;

import java.util.Objects;

/** Immutable settings supplied before a session's creation hook runs. */
public record SessionCreation<C>(int maxPlayers, C config) {
    public SessionCreation {
        if (maxPlayers < 1 || maxPlayers > 128)
            throw new IllegalArgumentException("Session capacity must be between 1 and 128");
        Objects.requireNonNull(config);
    }
}
