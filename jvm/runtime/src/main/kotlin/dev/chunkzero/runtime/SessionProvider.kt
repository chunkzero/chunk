package dev.chunkzero.runtime

/** Application JARs register one provider with Java's ServiceLoader. Factories create fresh session state. */
interface SessionProvider {
    fun sessions(): Map<String, () -> Session>
}
