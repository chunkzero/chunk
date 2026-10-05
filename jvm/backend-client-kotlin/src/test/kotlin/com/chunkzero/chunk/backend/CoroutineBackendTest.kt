package com.chunkzero.chunk.backend

import com.chunkzero.chunk.backend.api.BackendValues
import com.chunkzero.chunk.backend.api.JsonType
import com.chunkzero.chunk.backend.api.QueryRef
import com.chunkzero.chunk.backend.api.SessionId
import com.chunkzero.chunk.backend.client.BackendSession
import com.chunkzero.chunk.backend.client.SessionIdentity
import io.grpc.ManagedChannelBuilder
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.yield
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import tools.jackson.core.type.TypeReference
import java.time.Duration
import java.util.Optional
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit

class CoroutineBackendTest {
    private val nullType = JsonType.of(object : TypeReference<Void?>() {}, BackendValues::checkNull)
    private val integerType = JsonType.of(object : TypeReference<Long>() {}, BackendValues::checkInteger)

    @Test
    fun `closing a backend discards queued watch states in an independent collector`() =
        runBlocking {
            val channel = ManagedChannelBuilder.forAddress("127.0.0.1", 1).usePlaintext().build()
            val scheduler = Executors.newSingleThreadScheduledExecutor()
            val owner = SupervisorJob()
            val backend =
                CoroutineBackend(
                    BackendSession.overCore(
                        channel,
                        "test-credential-with-at-least-32-bytes",
                        "build",
                        SessionIdentity(SessionId("game"), "duels", Optional.empty()),
                        scheduler,
                        Duration.ofSeconds(5),
                    ),
                    CoroutineScope(owner),
                )
            try {
                var observed = 0
                var cancelled = false
                val collector =
                    launch(start = CoroutineStart.UNDISPATCHED) {
                        try {
                            backend.watch(QueryRef("read", nullType, integerType), null).collect {
                                observed++
                            }
                        } catch (_: CancellationException) {
                            cancelled = true
                        }
                    }
                // Run the producer so its initial stale state queues before the collector resumes.
                yield()
                backend.close()
                collector.join()
                assertTrue(cancelled)
                assertEquals(0, observed)
                assertTrue(owner.isActive)
            } finally {
                backend.close()
                owner.cancel()
                scheduler.shutdownNow()
                scheduler.awaitTermination(2, TimeUnit.SECONDS)
                channel.shutdownNow().awaitTermination(2, TimeUnit.SECONDS)
            }
        }
}
