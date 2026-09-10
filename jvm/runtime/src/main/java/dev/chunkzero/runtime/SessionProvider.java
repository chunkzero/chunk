package dev.chunkzero.runtime;

import java.util.Map;
import java.util.function.Supplier;

/** Application JARs register a provider with Java's ServiceLoader. Factories create fresh session state. */
public interface SessionProvider {
    Map<String, Supplier<Session>> sessions();
}
