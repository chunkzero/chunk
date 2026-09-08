package dev.chunkzero.example

import dev.chunkzero.backend.CoroutineBackend
import dev.chunkzero.backend.client.OperationId
import dev.chunkzero.backend.client.QueryResult
import dev.chunkzero.example.generated.BackendTypes
import dev.chunkzero.runtime.CoroutineSession
import dev.chunkzero.runtime.Session
import dev.chunkzero.runtime.SessionProvider
import dev.chunkzero.runtime.SessionScope
import net.kyori.adventure.text.Component
import net.kyori.adventure.text.format.NamedTextColor
import net.minestom.server.MinecraftServer
import net.minestom.server.command.builder.Command
import net.minestom.server.entity.Player
import net.minestom.server.instance.LightingChunk
import net.minestom.server.instance.block.Block
import net.minestom.server.tag.Tag

class ExampleSessions : SessionProvider {
    override fun sessions(): Map<String, () -> Session> {
        MinecraftServer.getCommandManager().register(
            Command("coin").apply {
                setDefaultExecutor { sender, _ -> (sender as? Player)?.getTag(coinAction)?.run() }
            },
        )
        return mapOf(
            "lobby" to { ExampleSession("Lobby", Block.GRASS_BLOCK) },
            "arena" to { ExampleSession("Arena", Block.SANDSTONE) },
        )
    }

    companion object {
        internal val coinAction: Tag<Runnable> = Tag.Transient("chunk-example-coin")
    }
}

private class ExampleSession(
    private val label: String,
    private val floor: Block,
) : CoroutineSession() {
    private lateinit var scope: SessionScope
    private val players = mutableMapOf<Player, PlayerData>()

    override suspend fun create(scope: SessionScope) {
        this.scope = scope
        requireNotNull(scope.backend) { "Example requires an environment backend" }
        scope.createInstance().apply {
            setChunkSupplier(::LightingChunk)
            setGenerator { it.modifier().fillHeight(0, 40, floor) }
        }
    }

    override suspend fun join(player: Player) {
        val backend = scope.coroutines.backend(requireNotNull(scope.backend), player)
        val data = PlayerData(backend)
        players[player] = data
        player.setTag(ExampleSessions.coinAction, Runnable { increment(player, data) })
        scope.coroutines.launch {
            backend
                .watch(
                    BackendTypes.`shared$players$stats`,
                    BackendTypes.`Fn$shared$players$stats$Args`(),
                ).collect { state ->
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
        val stats = backend.query(BackendTypes.`shared$players$stats`, BackendTypes.`Fn$shared$players$stats$Args`())
        backend.mutate(
            BackendTypes.`shared$players$join`,
            BackendTypes.`Fn$shared$players$join$Args`(),
            OperationId(scope.operationId(player, "join")),
        )
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
                data.backend.mutate(
                    BackendTypes.`shared$players$coin`,
                    BackendTypes.`Fn$shared$players$coin$Args`(),
                    OperationId(scope.operationId(player, "coin-${data.sequence}")),
                )
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
        val backend: CoroutineBackend,
    ) {
        var sequence = 0L
        var busy = false
    }
}
