# Arena example

A playable Java project: a medieval keep as the lobby, and a red-versus-blue king-of-the-hill arena that players queue
into through a gate. A leaderboard in the keep updates live as matches finish anywhere in the environment. The project
is Java only, with no Kotlin dependency, and compiles with `-Xlint:all -Werror`.

It shows what chunk is for:

- **Sessions on demand.** Each arena is a session of `arena/koth`. Control starts sessions as players queue, up to eight
  players each, and adds more as they fill.
- **Moving players.** Walking into the gate calls `scope.move(player, Destinations.Arena.koth)`; when a match ends, the
  arena moves everyone back with `Destinations.Lobby.main`. Players keep their connection.
- **One backend for everyone.** The arena records each result with one `recordMatch` mutation. Each lobby session
  watches the `leaderboard` query once, however many players it holds, so the board redraws as soon as a match is
  recorded.

## Run it

From the repository root, after `mise install`:

```sh
just toolchain
target/debug/chunk dev examples/arena
```

Join `localhost:25565` with a Minecraft Java Edition 26.2 client; add `--offline-logins` to test without Mojang
authentication. You spawn in the keep, facing the leaderboard. Walk through the gate to the south to queue for the
arena. A match starts ten seconds after both teams have a player. While only one team stands on the hill in the centre,
that team scores a point a second. The first team to 60 points wins, or the team ahead after five minutes. Swords and
dyed armour are handed out at the team camps. Teammates can't hurt each other, the fallen respawn after three seconds,
and the void below y = 40 kills.

In the arena, `/match` shows the score and `/lobby` leaves the match. Stats persist in the backend across runs.

## Project layout

| Path                                                        | Contents                                                                                       |
| ----------------------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| [`chunk.toml`](chunk.toml)                                  | Local environment: up to four JVMs of 1 GiB, each hosting four sessions                        |
| [`apps/scope.ts`](apps/scope.ts)                            | The server-list ping and routing every login to the lobby                                      |
| [`apps/lobby/app.ts`](apps/lobby/app.ts)                    | The `lobby` app and its `main` destination, 50 players per session                             |
| [`apps/arena/app.ts`](apps/arena/app.ts)                    | The `arena` app: the `koth` implementation with its rules as config, its destination, commands |
| [`server/schema/index.ts`](server/schema/index.ts)          | The `fighters` table, indexed by player and by rank                                            |
| [`server/stats.ts`](server/stats.ts)                        | The `recordMatch` mutation and the `leaderboard` and `mine` queries                            |
| [`server/sessions.ts`](server/sessions.ts)                  | The arena's `status` session method, which `/match` calls                                      |
| [`shared/`](shared)                                         | Polar world loading for both apps                                                              |
| [`apps/lobby/src/`](apps/lobby/src/main/java/example/lobby) | The lobby: spawn, the gate, and the leaderboard                                                |
| [`apps/arena/src/`](apps/arena/src/main/java/example/arena) | The arena: match rules, combat, the boss bar and titles, and recording results                 |
| `apps/*/src/main/resources/worlds/`                         | The `lobby.polar` and `arena.polar` worlds                                                     |

### The Java API

- [`Lobby.java`](apps/lobby/src/main/java/example/lobby/Lobby.java) and
  [`Arena.java`](apps/arena/src/main/java/example/arena/Arena.java) are each app's `main` and session provider. The
  arena implements the generated `ArenaSessionProviders.Koth` interface to receive the destination's config, and its
  [`ArenaSession`](apps/arena/src/main/java/example/arena/ArenaSession.java) implements the generated
  `SessionMethods.Arena.Koth.Status`.
- [`LobbyComponents`](apps/lobby/src/main/java/example/lobby/LobbyComponents.java) and
  [`ArenaComponents`](apps/arena/src/main/java/example/arena/ArenaComponents.java) declare a `PROCESS` component that
  parses the app's world once per JVM, and a `SESSION` component for the session's `BackendClient`.
- [`Leaderboard`](apps/lobby/src/main/java/example/lobby/Leaderboard.java) watches `leaderboard` through the session's
  client. [`LobbySession`](apps/lobby/src/main/java/example/lobby/LobbySession.java) binds a client to each joining
  player with `forPlayer` and reads their record with the `mine` query.
- [`Results`](apps/arena/src/main/java/example/arena/Results.java) calls `recordMatch` with one operation ID per match
  and retries under that ID, so a retry after a lost reply can't count a match twice.
- [`Portal`](apps/lobby/src/main/java/example/lobby/Portal.java) and `ArenaSession` move players and tell them when a
  move is refused.

### Worlds

Both worlds are 100 by 100 blocks of medieval build, stored in [Polar](https://github.com/hollow-cube/polar)'s format.
`shared/` reads them with Polar's reader and loads them through
[`PolarLoader`](shared/src/main/java/example/world/PolarLoader.java). That class is Polar 1.16.0's loader, adapted to
chunk's Minestom, which runs a `ServerProcess` instead of a global server, and trimmed to loading. It is MIT licensed;
see [`shared/POLAR-LICENSE`](shared/POLAR-LICENSE). Every session loads its own copy of its app's world, which it
discards when it ends.

## Tests

`examples/arena/gradlew test` runs [`MatchTest`](apps/arena/src/test/java/example/arena/MatchTest.java), which covers
the capture rule, both ways to win, and the countdown. `just consumers` builds this project from a source-only scratch
copy as the Java consumer fixture. It checks the release archive, the session registries and that no Kotlin classes
reach the apps.

## Local state

Local state lives in `examples/arena/.chunk/local`, as in the [Kotlin example](../local/README.md#local-state).
`target/debug/chunk clean examples/arena --data` resets the leaderboard.
