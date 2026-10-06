package com.chunkzero.chunk.runtime;

/** One app-owned service provider creates fresh session state for each instance of that app. */
@FunctionalInterface
public interface SessionProvider {
    Session create();
}
