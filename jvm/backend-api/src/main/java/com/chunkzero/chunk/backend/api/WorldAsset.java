package com.chunkzero.chunk.backend.api;

import java.util.Objects;

/** A world an app declares, as the generated {@code Worlds} constants name it. */
public record WorldAsset(String app, String name) {
    public WorldAsset {
        Objects.requireNonNull(app, "app");
        Objects.requireNonNull(name, "name");
    }
}
