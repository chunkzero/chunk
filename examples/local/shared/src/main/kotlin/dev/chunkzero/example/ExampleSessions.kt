package dev.chunkzero.example

import dev.chunkzero.backend.client.QueryResult
import dev.chunkzero.example.generated.CoroutineBackendClient
import dev.chunkzero.example.generated.SessionMethods
import dev.chunkzero.runtime.CoroutineSession
import dev.chunkzero.runtime.Session
import dev.chunkzero.runtime.SessionScope
import dev.chunkzero.runtime.coroutines
import kotlinx.coroutines.flow.distinctUntilChangedBy
import kotlinx.coroutines.launch
import net.kyori.adventure.text.Component
import net.kyori.adventure.text.format.NamedTextColor
import net.minestom.server.MinecraftServer
import net.minestom.server.command.builder.Command
import net.minestom.server.entity.Player
import net.minestom.server.instance.LightingChunk
import net.minestom.server.instance.block.Block
import net.minestom.server.tag.Tag

object ExampleSessions {
    internal val coinAction: Tag<Runnable> = Tag.Transient("chunk-example-coin")

    init {
        MinecraftServer.getCommandManager().register(
            Command("coin").apply {
                setDefaultExecutor { sender, _ -> (sender as? Player)?.getTag(coinAction)?.run() }
            },
        )
    }

    fun lobby() = LobbySession()

    fun arena(): Session = ExampleSession("Arena", Block.SANDSTONE)
}

class LobbySession :
    ExampleSession("Lobby", Block.GRASS_BLOCK),
    SessionMethods.Lobby.Default.Population {
    override fun population(args: SessionMethods.Lobby.Default.Population.Args): Long = playerCount().toLong()
}

open class ExampleSession(
    private val label: String,
    private val floor: Block,
) : CoroutineSession() {
    private lateinit var scope: SessionScope
    private val players = mutableMapOf<Player, PlayerData>()

    protected fun playerCount() = players.size

    override suspend fun create(scope: SessionScope) {
        this.scope = scope
        requireNotNull(scope.backend) { "Example requires an environment backend" }
        scope.createInstance().apply {
            setChunkSupplier(::LightingChunk)
            setGenerator { it.modifier().fillHeight(0, 40, floor) }
        }
    }

    override suspend fun join(player: Player) {
        val playerBackend = CoroutineBackendClient(scope.coroutines.backend(requireNotNull(scope.backend), player))
        val data = PlayerData(playerBackend)
        players[player] = data
        player.setTag(ExampleSessions.coinAction, Runnable { scope.onTick { increment(player, data) } })
        scope.coroutines.launch {
            playerBackend.shared.players
                .watchStats()
                .distinctUntilChangedBy { it.stale() to it.snapshot() }
                .collect { state ->
                    if (players[player] === data && player.isOnline) {
                        val result = state.snapshot().orElse(null)?.result()
                        val stats = if (result is QueryResult.Value) result.value() else null
                        val status = if (state.stale()) "reconnecting" else "live"
                        val message =
                            "$label | Coins: ${stats?.coins() ?: "?"} | " +
                                "Visits: ${stats?.visits() ?: "?"} | $status"
                        player.sendActionBar(
                            Component.text(message, if (state.stale()) NamedTextColor.YELLOW else NamedTextColor.GREEN),
                        )
                        player.sendMessage(Component.text(message))
                    }
                }
        }
        val stats = playerBackend.shared.players.stats()
        playerBackend.shared.players.join(operation = scope.operationId(player, "join"))
        player.sendMessage(Component.text("Welcome to $label. Saved coins: ${stats.coins()}. Use /coin to earn one."))
    }

    private fun increment(
        player: Player,
        data: PlayerData,
    ) {
        scope.coroutines.launch {
            if (players[player] !== data || !player.isOnline || data.busy) return@launch
            data.busy = true
            try {
                data.playerBackend.shared.players
                    .coin(operation = scope.operationId(player, "coin-${data.sequence}"))
                data.sequence++
            } catch (error: java.util.concurrent.CancellationException) {
                throw error
            } catch (_: Exception) {
                if (player.isOnline) {
                    player.sendMessage(
                        Component.text("Backend unavailable; use /coin again to retry this same reward."),
                    )
                }
            } finally {
                data.busy = false
            }
        }
    }

    override suspend fun leave(player: Player) {
        player.removeTag(ExampleSessions.coinAction)
        players.remove(player)
    }

    private class PlayerData(
        val playerBackend: CoroutineBackendClient,
    ) {
        var sequence = 0L
        var busy = false
    }
}
