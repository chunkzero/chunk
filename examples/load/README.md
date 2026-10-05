# Load example

A minimal project for load runs with [`chunk-bots`](../../crates/chunk-bots/README.md): every player goes to a flat
lobby of 128-player sessions, one per JVM. Each player's profile loads from the backend on join and saves every 60
seconds, and each JVM logs its average and slowest tick, with the players online, every 200 ticks
(`ticks: players=...`).

| Path                                                                               | Contents                                           |
| ---------------------------------------------------------------------------------- | -------------------------------------------------- |
| [`chunk.toml`](chunk.toml)                                                         | Local environment: up to eight JVMs of 1024 MiB    |
| [`apps/scope.ts`](apps/scope.ts)                                                   | Routes every login to the lobby                    |
| [`apps/lobby/app.ts`](apps/lobby/app.ts)                                           | The `lobby` app, 128 players per session           |
| [`server/players.ts`](server/players.ts)                                           | `load` and `save`, keyed by the calling player     |
| [`Lobby.kt`](apps/lobby/src/main/kotlin/com/chunkzero/chunk/example/load/Lobby.kt) | The session, its per-player saves and the tick log |

## Run it locally

From the repository root, after `just toolchain`:

```sh
target/debug/chunk dev examples/load --offline-logins --bind 127.0.0.1:25566 --control-bind 127.0.0.1:25568 --plain
cargo run --release -p chunk-bots -- --address 127.0.0.1:25566 --bots 200 --hold 120
```

JVM logs, with the tick lines, are under `examples/load/.chunk/local/`.
