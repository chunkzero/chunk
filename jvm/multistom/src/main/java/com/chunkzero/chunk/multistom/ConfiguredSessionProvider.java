package com.chunkzero.chunk.multistom;

import com.chunkzero.chunk.backend.api.JsonType;

/** Implement an app's generated provider interface to receive its validated creation config. */
public interface ConfiguredSessionProvider<C> extends SessionProvider {
    Session create(SessionCreation<C> creation);

    JsonType<C> configurationType();

    @Override
    default Session create() {
        throw new IllegalStateException("Configured session requires creation settings");
    }
}
