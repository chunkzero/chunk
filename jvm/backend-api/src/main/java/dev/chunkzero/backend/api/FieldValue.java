package dev.chunkzero.backend.api;

import java.util.Objects;

/** Absence is independent of a present value whose codec represents JSON null. */
public sealed interface FieldValue<T> permits FieldValue.Absent, FieldValue.Present {
    record Absent<T>() implements FieldValue<T> {}
    record Present<T>(T value) implements FieldValue<T> {
        public Present { Objects.requireNonNull(value); }
    }
    static <T> FieldValue<T> absent() { return new Absent<>(); }
    static <T> FieldValue<T> present(T value) { return new Present<>(value); }
}
