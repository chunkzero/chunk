package com.chunkzero.chunk.runtime;

import com.chunkzero.chunk.backend.api.SessionMethodRef;

import java.util.Objects;
import java.util.function.BiFunction;

/**
 * A generated direct call on sessions of type {@code S}. The dispatcher must check authority and
 * lifetime and enter the session's thread.
 */
public final class SessionMethodBinding<S, A, R> {
    private final SessionMethodRef<A, R> reference;
    private final Class<S> session;
    private final BiFunction<S, A, R> implementation;

    public SessionMethodBinding(
            SessionMethodRef<A, R> reference,
            Class<S> session,
            BiFunction<S, A, R> implementation) {
        this.reference = Objects.requireNonNull(reference);
        this.session = Objects.requireNonNull(session);
        this.implementation = Objects.requireNonNull(implementation);
    }

    public SessionMethodRef<A, R> reference() {
        return reference;
    }

    /** The session class the method is declared on. */
    public Class<S> session() {
        return session;
    }

    /**
     * Validates JSON on both sides of the synchronous gameplay call. Throws {@link
     * ClassCastException} if {@code target} is not an {@code S}.
     */
    public String invoke(Object target, String argumentsJson) {
        var typed = session.cast(Objects.requireNonNull(target));
        var args = reference.arguments().read(argumentsJson);
        return reference.result().write(implementation.apply(typed, args));
    }
}
