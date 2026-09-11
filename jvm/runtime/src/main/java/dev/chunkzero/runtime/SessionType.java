package dev.chunkzero.runtime;

import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;

/** Declares a session factory in the app's immutable build manifest. */
@Retention(RetentionPolicy.CLASS)
@Target(ElementType.TYPE)
public @interface SessionType {
    String value();

    String machineProfile() default "";

    int capacity() default 0;
}
