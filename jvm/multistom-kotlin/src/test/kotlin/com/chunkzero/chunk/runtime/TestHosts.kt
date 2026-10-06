package com.chunkzero.chunk.runtime

import chunk.sync.v1.Jvm.JvmSession

/** Test bridge to the package-private parts of [ChunkSessions]. */
object TestHosts {
    fun detached(handler: SessionHandler): ChunkSessions = ChunkSessions.detached(handler) { null }

    fun create(
        host: ChunkSessions,
        id: String,
        session: JvmSession,
    ) = host.create(id, session)

    fun finish(
        host: ChunkSessions,
        id: String,
        session: JvmSession,
    ) = host.finish(id, session)
}
