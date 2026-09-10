package dev.chunkzero.example.arena

import dev.chunkzero.example.ExampleSessions
import dev.chunkzero.runtime.SessionProvider

class ArenaSessions : SessionProvider {
    override fun create() = ExampleSessions.arena()
}
