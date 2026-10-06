package com.chunkzero.chunk.runtime;

import org.jetbrains.annotations.ApiStatus;

import java.util.Collection;

/** Generated app-local registration; factories are never discovered by scanning at runtime. */
@ApiStatus.Internal
public interface ComponentProvider {
    Collection<ComponentBinding<?>> components();
}
