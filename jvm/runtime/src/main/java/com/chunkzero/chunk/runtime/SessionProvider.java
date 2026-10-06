package com.chunkzero.chunk.runtime;

/**
 * Creates a fresh session of one {@link SessionType}, registered as an app-owned service. The host
 * decides what type of session {@code S} it accepts.
 */
@FunctionalInterface
public interface SessionProvider<S> {
    S create();
}
