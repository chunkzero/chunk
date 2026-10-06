package com.chunkzero.chunk.example.load

import com.chunkzero.chunk.example.load.generated.BackendTypes
import com.chunkzero.chunk.example.load.generated.CoroutineBackendClient
import com.chunkzero.chunk.multistom.ChunkMinestom
import com.chunkzero.chunk.multistom.CoroutineSession
import com.chunkzero.chunk.multistom.SessionProvider
import com.chunkzero.chunk.multistom.SessionScope
import com.chunkzero.chunk.multistom.coroutines
import com.chunkzero.chunk.multistom.own
import com.chunkzero.chunk.runtime.ChunkProcess
import com.chunkzero.chunk.runtime.SessionType
import kotlinx.coroutines.launch
import net.minestom.server.ServerProcess
import net.minestom.server.entity.Player
import net.minestom.server.event.server.ServerTickMonitorEvent
import net.minestom.server.instance.LightingChunk
import net.minestom.server.instance.block.Block
import kotlin.time.Duration.Companion.seconds
import kotlin.time.toJavaDuration

private const val SAVE_SECONDS = 60L
private const val TICKS_PER_LOG = 200

@SessionType("default")
class LobbySessions : SessionProvider {
    override fun create() = LobbySession()
}

/** A flat lobby that loads each player's profile on join and saves it every minute. */
class LobbySession : CoroutineSession() {
    private lateinit var scope: SessionScope

    override suspend fun create(scope: SessionScope) {
        this.scope = scope
        requireNotNull(scope.backend) { "The load example requires an environment backend" }
        scope.createInstance().apply {
            setChunkSupplier(::LightingChunk)
            setGenerator { it.modifier().fillHeight(0, 40, Block.GRASS_BLOCK) }
        }
    }

    override suspend fun join(player: Player) {
        val backend = CoroutineBackendClient(scope.coroutines.backend(requireNotNull(scope.backend), player))
        backend.shared.players.load()
        var saves = 0
        val save = {
            val operation = scope.operationId(player, "save-${++saves}")
            scope.coroutines.launch {
                runCatching {
                    backend.shared.players.save(
                        BackendTypes.Shared.Players.SaveArgs(SAVE_SECONDS),
                        operation,
                    )
                }.onFailure { LOG.log(System.Logger.Level.WARNING, "save failed for ${player.username}", it) }
            }
        }
        val interval = SAVE_SECONDS.seconds.toJavaDuration()
        val task =
            scope.scheduler
                .buildTask { save() }
                .delay(interval)
                .repeat(interval)
                .schedule()
        scope.own(player, task)
    }
}

private val LOG = System.getLogger("load")

/** Logs the average and slowest tick, and the players online, every ten seconds. */
private class TickLog(
    private val server: ServerProcess,
) {
    private var ticks = 0
    private var total = 0.0
    private var slowest = 0.0

    fun record(event: ServerTickMonitorEvent) {
        val milliseconds = event.tickMonitor.tickTime
        ticks++
        total += milliseconds
        slowest = maxOf(slowest, milliseconds)
        if (ticks == TICKS_PER_LOG) {
            val players = server.connectionManager().onlinePlayerCount
            LOG.log(
                System.Logger.Level.INFO,
                "ticks: players=$players avg=%.2fms max=%.2fms".format(total / ticks, slowest),
            )
            ticks = 0
            total = 0.0
            slowest = 0.0
        }
    }
}

fun main() {
    ChunkProcess.connect().use { chunk ->
        val server = ServerProcess.create()
        val ticks = TickLog(server)
        server.eventHandler().addListener(ServerTickMonitorEvent::class.java, ticks::record)
        ChunkMinestom.attach(chunk, server).use { minestom ->
            minestom.start()
            chunk.ready()
            chunk.awaitShutdown()
        }
    }
}
