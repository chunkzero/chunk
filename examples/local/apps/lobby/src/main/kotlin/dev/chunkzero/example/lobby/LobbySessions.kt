package dev.chunkzero.example.lobby

import dev.chunkzero.example.ExampleSessions
import dev.chunkzero.runtime.SessionProvider

class LobbySessions : SessionProvider {
    override fun create() = ExampleSessions.lobby()
}
