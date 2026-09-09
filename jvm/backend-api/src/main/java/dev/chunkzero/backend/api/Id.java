package dev.chunkzero.backend.api;

import java.util.Objects;

/** The generated table marker keeps IDs from unrelated tables distinct. */
public record Id<T>(String value) {
    public Id { Objects.requireNonNull(value); }
}
