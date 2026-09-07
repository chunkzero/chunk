package dev.chunkzero.example

import com.google.protobuf.ByteString
import dev.chunkzero.backend.Query
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
import java.util.concurrent.CompletableFuture
import java.util.concurrent.CompletionStage

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
) : Session() {
    private lateinit var scope: SessionScope
    private val players = mutableMapOf<Player, PlayerData>()

    override fun onCreate(scope: SessionScope): CompletionStage<Unit> {
        this.scope = scope
        requireNotNull(scope.backend) { "Example requires an environment backend" }
        scope.createInstance().apply {
            setChunkSupplier(::LightingChunk)
            setGenerator { it.modifier().fillHeight(0, 40, floor) }
        }
        scope.own(
            AutoCloseable {
                players.values.forEach { it.subscription?.close() }
                players.clear()
            },
        )
        return CompletableFuture.completedFuture(Unit)
    }

    override fun onJoin(player: Player): CompletionStage<Unit> {
        val data = PlayerData()
        players[player] = data
        val backend = requireNotNull(scope.backend)
        player.setTag(ExampleSessions.coinAction, Runnable { scope.onTick { increment(player, data) } })
        data.subscription =
            backend.subscribe(listOf(query("players/balance", player), query("players/visits", player))) { state ->
                scope.onTick {
                    if (players[player] === data && player.isOnline) {
                        val values = state.snapshot?.resultsJsonList?.map { it.toStringUtf8().toLong() }
                        val coins = values?.get(0) ?: "?"
                        val visits = values?.get(1) ?: "?"
                        val status = if (state.stale) "reconnecting" else "live"
                        val message = "$label | Coins: $coins | Visits: $visits | $status"
                        player.sendActionBar(
                            Component.text(message, if (state.stale) NamedTextColor.YELLOW else NamedTextColor.GREEN),
                        )
                        player.sendMessage(Component.text(message))
                    }
                }
            }
        return backend.call(query("players/balance", player), "").thenCompose { balance ->
            backend.call(query("players/join", player), scope.operationId(player, "join")).thenCompose {
                scope.onTick {
                    if (players[player] === data && player.isOnline) {
                        player.sendMessage(
                            Component.text(
                                "Welcome to $label. Saved coins: ${balance.resultJson.toStringUtf8()}. " +
                                    "Use /coin to earn one.",
                            ),
                        )
                    }
                }
            }
        }
    }

    private fun increment(
        player: Player,
        data: PlayerData,
    ) {
        if (players[player] !== data || !player.isOnline || data.busy) return
        data.busy = true
        val operation = scope.operationId(player, "coin-${data.sequence}")
        requireNotNull(scope.backend).call(query("players/coin", player), operation).whenComplete { _, error ->
            scope.onTick {
                if (players[player] === data && player.isOnline) {
                    data.busy = false
                    if (error == null) {
                        data.sequence++
                    } else {
                        player.sendMessage(
                            Component.text("Backend unavailable; use /coin again to retry this same reward."),
                        )
                    }
                }
            }
        }
    }

    override fun onLeave(player: Player): CompletionStage<Unit> {
        player.removeTag(ExampleSessions.coinAction)
        players.remove(player)?.subscription?.close()
        return CompletableFuture.completedFuture(Unit)
    }

    private fun query(
        function: String,
        player: Player,
    ) = Query(function, ByteString.copyFromUtf8("{\"uuid\":\"${player.uuid}\"}"))

    private class PlayerData {
        var subscription: AutoCloseable? = null
        var sequence = 0L
        var busy = false
    }
}
