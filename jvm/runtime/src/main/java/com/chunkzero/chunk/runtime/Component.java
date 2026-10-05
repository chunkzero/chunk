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
}
