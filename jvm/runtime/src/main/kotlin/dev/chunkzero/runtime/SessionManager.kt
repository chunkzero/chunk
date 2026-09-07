package dev.chunkzero.runtime

import chunk.v1.Supervision.SessionCommand
import chunk.v1.Supervision.SessionInventory
import chunk.v1.Supervision.SessionPhase
import dev.chunkzero.backend.BackendClient
import net.minestom.server.entity.Player
import java.util.concurrent.CompletableFuture
import java.util.concurrent.CompletionStage
import java.util.concurrent.ConcurrentHashMap

internal class SessionManager(
    val ticks: TickExecutor,
    private val factories: Map<String, () -> Session>,
    private val backend: ((String, Long) -> BackendClient)? = null,
) {
    private val sessions = ConcurrentHashMap<String, ManagedSession>()
    var withdraw: (String) -> CompletionStage<Unit> = { CompletableFuture.completedFuture(Unit) }

    fun create(command: SessionCommand): CompletableFuture<SessionInventory> =
        ticks
            .submit {
                require(command.session.id.matches(Regex("[A-Za-z0-9_-]{1,128}")) && command.generation > 0)
                require(command.operationId.length in 1..128 && command.capacity in 1..128)
                val previous = sessions[command.session.id]
                if (previous != null) {
                    require(previous.command == command) { "Session creation changed" }
                    previous
                } else {
                    check(sessions.size < 256) { "Session history full" }
                    val factory = requireNotNull(factories[command.sessionType]) { "Unknown session type" }
                    ManagedSession(command, factory()).also {
                        sessions[command.session.id] = it
                        it.start()
                    }
                }
            }.thenCompose { it.ready.thenApply { _ -> it.inventory() } }

    fun finish(command: SessionCommand): CompletableFuture<SessionInventory> =
        ticks
            .submit {
                val session = requireNotNull(sessions[command.session.id])
                require(session.command.generation == command.generation)
                session
            }.thenCompose { it.finish().thenApply { _ -> it.inventory() } }

    fun get(
        id: String,
        generation: Long,
    ): ManagedSession =
        requireNotNull(sessions[id]).also {
            require(it.command.generation == generation) { "Stale session generation" }
            check(it.phase == SessionPhase.SESSION_PHASE_READY) { "Session unavailable" }
        }

    fun inventory() = sessions.values.map { it.inventory() }

    inner class ManagedSession(
        val command: SessionCommand,
        private val behavior: Session,
    ) {
        @Volatile var phase = SessionPhase.SESSION_PHASE_STARTING
            private set
        val scope =
            SessionScope(command.session.id, command.generation, ticks, {
                finish()
            }, backend?.invoke(command.session.id, command.generation))
        val ready = CompletableFuture<Unit>()
        private val ended = CompletableFuture<Unit>()
        private var finishing = false
        private var creationFailure: Throwable? = null

        fun start() {
            invoke { behavior.onCreate(scope) }.whenComplete { _, error ->
                ticks.submit {
                    if (error == null && scope.instances.isNotEmpty()) {
                        if (!finishing) phase = SessionPhase.SESSION_PHASE_READY
                        ready.complete(Unit)
                    } else {
                        phase = SessionPhase.SESSION_PHASE_FAILED
                        creationFailure = error ?: IllegalStateException("Session has no instances")
                        ready.completeExceptionally(requireNotNull(creationFailure))
                        finish()
                    }
                }
            }
        }

        fun join(player: Player): CompletableFuture<Unit> =
            ticks
                .submit {
                    check(phase == SessionPhase.SESSION_PHASE_READY)
                    scope.players.add(player)
                    invoke { behavior.onJoin(player) }
                }.thenCompose { it }

        fun leave(player: Player): CompletableFuture<Unit> =
            ticks
                .submit {
                    if (scope.players.remove(
                            player,
                        )
                    ) {
                        invoke { behavior.onLeave(player) }
                    } else {
                        CompletableFuture.completedFuture(Unit)
                    }
                }.thenCompose { it }

        fun finish(): CompletableFuture<Unit> {
            ticks.submit {
                if (!finishing) {
                    finishing = true
                    phase = SessionPhase.SESSION_PHASE_ENDING
                    // Creation must settle before scoped resources can be disposed.
                    ready
                        .handle { _, _ -> Unit }
                        .thenCompose { withdraw(command.session.id) }
                        .thenCompose { ticks.submit { invoke { behavior.onFinish() } }.thenCompose { it } }
                        .whenComplete { _, error ->
                            ticks.submit {
                                try {
                                    scope.dispose()
                                    val failure = error ?: creationFailure
                                    phase =
                                        if (failure ==
                                            null
                                        ) {
                                            SessionPhase.SESSION_PHASE_ENDED
                                        } else {
                                            SessionPhase.SESSION_PHASE_FAILED
                                        }
                                    if (failure == null) ended.complete(Unit) else ended.completeExceptionally(failure)
                                } catch (failure: Exception) {
                                    phase = SessionPhase.SESSION_PHASE_FAILED
                                    ended.completeExceptionally(failure)
                                }
                            }
                        }
                }
            }
            return ended
        }

        fun inventory(): SessionInventory =
            SessionInventory
                .newBuilder()
                .setSession(command.session)
                .setGeneration(command.generation)
                .setSessionType(command.sessionType)
                .setCapacity(command.capacity)
                .setPhase(phase)
                .setAttached(scope.players.size)
                .build()
    }
}

private fun invoke(block: () -> CompletionStage<Unit>): CompletableFuture<Unit> =
    try {
        block().toCompletableFuture()
    } catch (error: Exception) {
        CompletableFuture.failedFuture(error)
    }
