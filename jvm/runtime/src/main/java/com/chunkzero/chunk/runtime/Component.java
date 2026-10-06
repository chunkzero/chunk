package com.chunkzero.chunk.runtime;

import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;

/**
 * Declares a public static factory. Its exact return type identifies the component and its
 * parameters identify dependencies. The app build validates and wires these declarations.
 */
@Retention(RetentionPolicy.CLASS)
@Target(ElementType.METHOD)
public @interface Component {
    Scope value();

    enum Scope {
        PROCESS,
        SESSION
    }

    /**
     * Marks a type the host supplies to session-scoped component factories, such as a handle to the
     * session. Factories may take it as a dependency; no factory may provide it.
     */
    @Retention(RetentionPolicy.RUNTIME)
    @Target(ElementType.TYPE)
    @interface Supplied {}
}
