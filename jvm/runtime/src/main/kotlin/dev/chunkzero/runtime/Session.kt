package dev.chunkzero.runtime

import dev.chunkzero.backend.BackendClient
import net.minestom.server.MinecraftServer
import net.minestom.server.entity.Player
import net.minestom.server.event.EventFilter
import net.minestom.server.event.EventNode
import net.minestom.server.instance.InstanceContainer
import java.time.Duration
import java.util.IdentityHashMap
import java.util.concurrent.CompletableFuture
import java.util.concurrent.CompletionStage
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.ConcurrentLinkedQueue
import java.util.concurrent.CopyOnWriteArrayList

/** Hooks run on the process tick thread; asynchronous continuations use [SessionScope.onTick]. */
abstract class Session {
    open fun onCreate(scope: SessionScope): CompletionStage<Unit> = CompletableFuture.completedFuture(Unit)

    open fun onJoin(player: Player): CompletionStage<Unit> = CompletableFuture.completedFuture(Unit)

    open fun onLeave(player: Player): CompletionStage<Unit> = CompletableFuture.completedFuture(Unit)

    open fun onFinish(): CompletionStage<Unit> = CompletableFuture.completedFuture(Unit)
}

class SessionScope internal constructor(
    val id: String,
    val generation: Long,
    private val ticks: TickExecutor,
    private val requestFinish: () -> CompletionStage<Unit>,
    val backend: BackendClient?,
) {
    private val ownedInstances = CopyOnWriteArrayList<InstanceContainer>()
    private val resources = mutableListOf<AutoCloseable>()
    private val playerResources = IdentityHashMap<Player, MutableList<AutoCloseable>>()
    val coroutines by lazy { own(SessionCoroutines(this, ticks)) }
    internal val players =
        ConcurrentHashMap
            .newKeySet<Player>()
    private var disposed = false
    val events = EventNode.value("session-$id-$generation", EventFilter.PLAYER) { players.contains(it) }
    val instances: List<InstanceContainer> get() = ownedInstances.toList()

    init {
        MinecraftServer.getGlobalEventHandler().addChild(events)
    }

    fun createInstance(): InstanceContainer {
        ticks.checkThread()
        check(!disposed)
        check(ownedInstances.size < 16) { "Session instance limit reached" }
        return MinecraftServer.getInstanceManager().createInstanceContainer().also { ownedInstances.add(it) }
    }

    /** Register subscriptions and other session resources for disposal. */
    fun <T : AutoCloseable> own(resource: T): T {
        ticks.checkThread()
        if (disposed) {
            resource.close()
            error("Session disposed")
        }
        if (resources.size + playerResources.values.sumOf { it.size } >= 1024) {
            resource.close()
            error("Session resource limit reached")
        }
        resources.add(resource)
        return resource
    }

    /** Player resources are keyed by the admitted object, keeping replacements independent. */
    fun <T : AutoCloseable> own(
        player: Player,
        resource: T,
    ): T {
        ticks.checkThread()
        if (disposed || player !in players || resources.size + playerResources.values.sumOf { it.size } >= 1024) {
            resource.close()
            error("Player scope unavailable")
        }
        playerResources.getOrPut(player) { mutableListOf() }.add(resource)
        return resource
    }

    internal fun releasePlayer(player: Player) {
        ticks.checkThread()
        var failure: Exception? = null
        playerResources.remove(player)?.asReversed()?.forEach {
            try {
                it.close()
            } catch (error: Exception) {
                failure = error
            }
        }
        failure?.let { throw it }
    }

    fun <T> onTick(action: () -> T): CompletableFuture<T> =
        ticks.submit {
            check(!disposed)
            action()
        }

    /** Stable mutation identity for one action on this exact player delivery. */
    fun operationId(
        player: Player,
        action: String,
    ): String {
        require(action.matches(Regex("[A-Za-z0-9_-]{1,32}")))
        require(players.contains(player))
        val binding = (player as ManagedPlayer).binding
        return "$id/${player.uuid}/${binding.ownerGeneration}/$action"
    }

    fun finish(): CompletionStage<Unit> = requestFinish()

    fun repeatEvery(
        interval: Duration,
        action: () -> Unit,
    ): AutoCloseable {
        ticks.checkThread()
        require(!interval.isNegative && !interval.isZero)
        val task =
            MinecraftServer
                .getSchedulerManager()
                .buildTask {
                    if (!disposed) action()
                }.repeat(interval)
                .schedule()
        return own(AutoCloseable { task.cancel() })
    }

    internal fun dispose() {
        ticks.checkThread()
        if (disposed) return
        check(players.isEmpty())
        disposed = true
        MinecraftServer.getGlobalEventHandler().removeChild(events)
        var failure: Exception? = null
        resources.asReversed().forEach {
            try {
                it.close()
            } catch (error: Exception) {
                failure = error
            }
        }
        ownedInstances.forEach { MinecraftServer.getInstanceManager().unregisterInstance(it) }
        failure?.let { throw it }
    }
}

internal class TickExecutor {
    private val pending = ConcurrentLinkedQueue<() -> Unit>()
    private var thread: Thread? = null

    fun isCurrentThread() = Thread.currentThread() === thread

    fun checkThread() = check(isCurrentThread()) { "Use SessionScope.onTick for world changes" }

    fun <T> submit(action: () -> T): CompletableFuture<T> {
        val result = CompletableFuture<T>()
        pending.add {
            try {
                result.complete(action())
            } catch (error: Exception) {
                result.completeExceptionally(error)
            }
        }
        return result
    }

    fun flush() {
        thread = Thread.currentThread()
        repeat(pending.size) { pending.poll()?.invoke() }
    }
}
