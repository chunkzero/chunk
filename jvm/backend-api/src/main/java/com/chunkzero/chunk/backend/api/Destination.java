package com.chunkzero.chunk.backend.api;

import java.util.Objects;

/** A place an app sends players, as the generated {@code Destinations} constants name it. */
public record Destination(String key, String sessionType, String machineProfile) {
    public Destination {
        Objects.requireNonNull(key, "key");
        Objects.requireNonNull(sessionType, "sessionType");
        Objects.requireNonNull(machineProfile, "machineProfile");
    }
}
