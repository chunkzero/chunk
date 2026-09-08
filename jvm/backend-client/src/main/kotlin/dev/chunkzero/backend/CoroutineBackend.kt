package dev.chunkzero.backend

import dev.chunkzero.backend.api.MutationRef
import dev.chunkzero.backend.api.QueryRef
import dev.chunkzero.backend.client.BackendSession
import dev.chunkzero.backend.client.OperationId
import dev.chunkzero.backend.client.WatchState
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.channels.awaitClose
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.buffer
import kotlinx.coroutines.flow.callbackFlow
import kotlinx.coroutines.future.await

/** Suspensions resume in their caller's coroutine context; managed sessions supply a tick dispatcher. */
class CoroutineBackend(
    private val client: BackendSession,
    owner: CoroutineScope,
) : AutoCloseable {
    private val job = SupervisorJob(requireNotNull(owner.coroutineContext[Job]) { "Backend requires an owned scope" })

    init {
        job.invokeOnCompletion { client.close() }
    }

    suspend fun <A, R> query(
        reference: QueryRef<A, R>,
        arguments: A,
    ): R {
        check(job.isActive) { "Backend scope closed" }
        return client.query(reference, arguments).await()
    }

    suspend fun <A, R> mutate(
        reference: MutationRef<A, R>,
        arguments: A,
        operation: OperationId,
    ): R {
        check(job.isActive) { "Backend scope closed" }
        return client.mutate(reference, arguments, operation).await()
    }

    /** A slow collector fails at the buffer bound rather than silently losing stale transitions. */
    fun <A, R> watch(
        reference: QueryRef<A, R>,
        arguments: A,
    ): Flow<WatchState<R>> =
        callbackFlow {
            val stopped = job.invokeOnCompletion { close(CancellationException("Backend scope closed")) }
            try {
                if (!job.isActive) return@callbackFlow
                val subscription =
                    client.watch(reference, arguments) {
                        if (trySend(it).isFailure) close(IllegalStateException("Backend watch buffer exhausted"))
                    }
                awaitClose { subscription.close() }
            } finally {
                stopped.dispose()
            }
        }.buffer(64)

    override fun close() {
        job.cancel()
        client.close()
    }
}
