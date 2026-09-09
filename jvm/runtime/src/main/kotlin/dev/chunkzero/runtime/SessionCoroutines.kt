package dev.chunkzero.runtime

import dev.chunkzero.backend.CoroutineBackend
import dev.chunkzero.backend.api.PlayerId
import dev.chunkzero.backend.client.BackendSession
import kotlinx.coroutines.CoroutineDispatcher
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.future.future
import kotlinx.coroutines.launch
import net.minestom.server.entity.Player
import java.util.concurrent.CompletableFuture
import kotlin.coroutines.CoroutineContext

/** Session-owned work always resumes on the process tick thread unless explicitly moved elsewhere. */
class SessionCoroutines internal constructor(
    private val session: SessionScope,
    ticks: TickExecutor,
) : AutoCloseable {
    private val job = SupervisorJob()
    private val scope =
        CoroutineScope(
            job +
                object : CoroutineDispatcher() {
                    override fun isDispatchNeeded(context: CoroutineContext) = !ticks.isCurrentThread()

                    override fun dispatch(
                        context: CoroutineContext,
                        block: Runnable,
                    ) {
                        ticks.submit { block.run() }
                    }
                },
        )

    fun <T> future(block: suspend CoroutineScope.() -> T): CompletableFuture<T> = scope.future(block = block)

    fun launch(block: suspend CoroutineScope.() -> Unit): Job = scope.launch(block = block)

    fun backend(client: BackendSession): CoroutineBackend = session.own(CoroutineBackend(client, scope))

    /** Identity comes from the admitted Player object; leaving disposes this child independently. */
    fun backend(
        client: BackendSession,
        player: Player,
    ): CoroutineBackend =
        session.own(player, CoroutineBackend(client.forPlayer(PlayerId(player.uuid.toString())), scope))

    override fun close() {
        job.cancel()
    }
}

/** Finish hooks settle before the manager disposes backend scopes, so result mutations can complete. */
abstract class CoroutineSession : Session() {
    private lateinit var coroutines: SessionCoroutines

    final override fun onCreate(scope: SessionScope): CompletableFuture<Unit> {
        coroutines = scope.coroutines
        return coroutines.future { create(scope) }
    }

    final override fun onJoin(player: Player): CompletableFuture<Unit> = coroutines.future { join(player) }

    final override fun onLeave(player: Player): CompletableFuture<Unit> = coroutines.future { leave(player) }

    final override fun onFinish(): CompletableFuture<Unit> = coroutines.future { finish() }

    open suspend fun create(scope: SessionScope) {}

    open suspend fun join(player: Player) {}

    open suspend fun leave(player: Player) {}

    open suspend fun finish() {}
}
