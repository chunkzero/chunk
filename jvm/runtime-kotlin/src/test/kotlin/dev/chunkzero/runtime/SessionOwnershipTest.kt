package dev.chunkzero.runtime

import kotlinx.coroutines.awaitCancellation
import net.minestom.server.MinecraftServer
import org.junit.jupiter.api.Assertions.assertSame
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import java.util.concurrent.CompletableFuture
import java.util.concurrent.CompletionException

class SessionOwnershipTest {
    @Test
    fun `one coroutine owner respects tick access disposal and lifecycle cancellation`() {
        MinecraftServer.init()
        val ticks = TickExecutor()
        ticks.flush()
        val scope = SessionScope("owned", 1, ticks, { CompletableFuture.completedFuture(null) }, null)
        try {
            val coroutines = scope.coroutines
            assertSame(coroutines, scope.coroutines)
            val offThread = CompletableFuture.supplyAsync { scope.coroutines }
            assertThrows(CompletionException::class.java) { offThread.join() }

            var cancelled = false
            val session =
                object : CoroutineSession() {
                    override suspend fun create(scope: SessionScope) {
                        try {
                            awaitCancellation()
                        } finally {
                            ticks.checkThread()
                            cancelled = true
                        }
                    }
                }
            val creating = session.onCreate(scope)
            creating.cancel(false)
            assertTrue(cancelled, "Cancelling the lifecycle future must cancel its suspended hook")

            val background = coroutines.launch { awaitCancellation() }
            scope.dispose()
            assertTrue(background.isCancelled)
            assertThrows(IllegalStateException::class.java) { scope.coroutines }
        } finally {
            scope.dispose()
            MinecraftServer.process().stop()
        }
    }
}
