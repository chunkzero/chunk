package com.chunkzero.chunk.runtime;

import com.chunkzero.chunk.backend.api.SessionMethodRef;

import java.util.Objects;
import java.util.function.BiFunction;

/**
 * A generated direct call. The dispatcher must check authority and lifetime and enter the tick
 * thread.
 */
public final class SessionMethodBinding<A, R> {
    private final SessionMethodRef<A, R> reference;
    private final BiFunction<Session, A, R> implementation;

    public SessionMethodBinding(
            SessionMethodRef<A, R> reference, BiFunction<Session, A, R> implementation) {
        this.reference = Objects.requireNonNull(reference);
        this.implementation = Objects.requireNonNull(implementation);
    }

    public SessionMethodRef<A, R> reference() {
        return reference;
    }

    /** Validates JSON on both sides of the synchronous gameplay call. */
    public String invoke(Session session, String arguments) {
        var args = reference.arguments().read(arguments);
        return reference
                .result()
                .write(implementation.apply(Objects.requireNonNull(session), args));
    }
}
