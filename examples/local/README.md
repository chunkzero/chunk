# Kotlin example

A playable project: a grass lobby and sandstone arenas written in Kotlin, with a TypeScript backend that keeps each
player's coins and visits. It is what `just local` runs, and it exercises most of the platform: routing and admission
hooks, backend commands, a session method, typed destination config, reactive watches and moving players between
sessions.

## Run it

From the repository root, after `mise install`:

```sh
just local
```

This builds the development CLI, installs the pinned TypeScript compiler, then runs `chunk dev examples/local`, which
builds the release and starts the backend, control and the gateway in one process. Join `localhost:25565` with a
signed-in Minecraft Java Edition 26.2 client; add `--offline-logins` (`just local --offline-logins`) to test without
Mojang authentication. Press `q` or Ctrl-C to stop everything.

In game:

- `/coin` is a Minestom command in the JVM. It runs the `coin` mutation, and the chat and action bar update from a watch
  on `stats`. Joining a session also increments your visits.
- `/hello <message>` and `/travel lobby|arena|large` are backend commands available everywhere. `/travel` moves you
  through the same admission and capacity checks as any other move.
- `/population`, in the lobby only, is a backend command that calls the lobby session's `population` session method.

Worlds live only in the JVMs; coins and visits persist in the backend across runs.

## Project layout

| Path                                                     | Contents                                                                                                                                             |
| -------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| [`chunk.toml`](chunk.toml)                               | Local environment: 16 players per session, up to four JVMs, and machine profiles `local` (512 MiB) and `large` (1024 MiB), each hosting two sessions |
| [`apps/scope.ts`](apps/scope.ts)                         | Root hooks (admission, server ping, routing to the lobby) and `/hello`, `/travel`                                                                    |
| [`apps/lobby/app.ts`](apps/lobby/app.ts)                 | The `lobby` app, its `main` destination and `/population`                                                                                            |
| [`apps/games/arena/app.ts`](apps/games/arena/app.ts)     | The `arena` app, with a `label` config and `standard` and `large` destinations                                                                       |
| [`server/schema/index.ts`](server/schema/index.ts)       | The `profiles` and `settings` tables                                                                                                                 |
| [`server/players.ts`](server/players.ts)                 | `stats`, `join` and `coin`, keyed by the calling player                                                                                              |
| [`server/proxy.ts`](server/proxy.ts)                     | The status and admission queries the root hooks call                                                                                                 |
| [`server/session-methods.ts`](server/session-methods.ts) | The `population` session method                                                                                                                      |
| [`shared/`](shared)                                      | Gameplay shared by both apps, in a plain Gradle project                                                                                              |
| `apps/*/src/`                                            | Each app's `main` and `@SessionType("default")` provider                                                                                             |

The `large` arena destination reuses the arena's `default` implementation with a different config, 32 players and the
`large` profile, so the arena has a single provider. [`settings.gradle.kts`](settings.gradle.kts) builds the Gradle
plugin and JVM libraries from this checkout and points the plugin at `target/debug/chunk`.

## Moving players

The terminal UI's Players tab lists connected players and shows the selected player's UUID; press `m` to move them. From
another terminal, the same works through `just players`:

```sh
just players --player <uuid> move --session-type arena/default --key arena
just players --player <uuid> move --session-type arena/default --key arena-large --machine-profile large
just players --player <uuid> drain --timeout-seconds 60
```

Moves keep the player's connection. Drain stops placing players on the player's current JVM, moves its players
elsewhere, and stops it once empty or at the deadline. Each command prints an operation ID; pass it back with
`--operation <id>` to retry a command whose outcome is unknown.
`target/debug/chunk nodes --control-file examples/local/.chunk/local/control.json list` shows each JVM with its health.

## Changing it

`chunk dev` watches the sources and rebuilds on change. New players go to the new release. After a backend-only change,
players already in a session stay there until they leave; after a JVM change they are disconnected after 30 seconds
(`--drain-seconds`). Press `r` to rebuild and restart everything at once.

For editor support without starting anything, run `target/debug/chunk codegen examples/local`, which materializes the
SDK under `examples/local/.chunk/`.

`examples/local/gradlew test` runs the `shared` project's test, which drives `/coin` through a real Minestom process
against an in-process fake core, across moves and rejoins.

## Local state

Local state lives in `examples/local/.chunk/local`: backend data under `backend/`, JVM logs, and connection records that
contain private credentials, so don't share that directory. `target/debug/chunk clean examples/local` removes build
output and state but keeps backend data; add `--data` to reset it too. The gateway listens on `127.0.0.1:25565` and
control on `127.0.0.1:25567`; `just local --bind ... --control-bind ... --state ...` picks other loopback addresses or
another state directory.
