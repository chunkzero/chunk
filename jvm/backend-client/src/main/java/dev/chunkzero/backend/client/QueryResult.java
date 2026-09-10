package dev.chunkzero.backend.client;

import java.util.Objects;

public sealed interface QueryResult<T> permits QueryResult.Value, QueryResult.Failure {
    T valueOrThrow();

    record Value<T>(T value) implements QueryResult<T> {
        public Value {
            Objects.requireNonNull(value);
        }

        public T valueOrThrow() {
            return value;
        }
    }

    record Failure<T>(String message) implements QueryResult<T> {
        public Failure {
            Objects.requireNonNull(message);
        }

        public T valueOrThrow() {
            throw new IllegalStateException(message);
        }
    }
}
