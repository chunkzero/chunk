package dev.chunkzero.runtime;

import org.jetbrains.annotations.ApiStatus;

import java.util.List;
import java.util.Objects;

/** A direct factory call emitted by the app build. */
@ApiStatus.Internal
public record ComponentBinding<T>(
        Class<T> type, Component.Scope scope, List<Class<?>> dependencies, Factory<T> factory) {
    public ComponentBinding {
        Objects.requireNonNull(type);
        Objects.requireNonNull(scope);
        dependencies = List.copyOf(dependencies);
        Objects.requireNonNull(factory);
    }

    @FunctionalInterface
    public interface Factory<T> {
        T create(Object[] dependencies) throws Exception;
    }
}
