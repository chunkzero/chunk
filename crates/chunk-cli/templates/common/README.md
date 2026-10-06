# @PROJECT_NAME@

A [chunk](https://github.com/chunkzero/chunk) project: a lobby app with Java or Kotlin gameplay on Minestom and a typed
TypeScript backend.

## Run it

From this directory:

```sh
chunk codegen
chunk build
chunk dev
```

- `codegen` sets up editor support for the TypeScript backend. It generates the SDK and typed bindings under `.chunk/`
  and adds import mappings to `package.json` (plus a `tsconfig.json` if there is none). Commit those two files.
- `build` compiles the backend and the apps and writes a release to `dist/`: a directory and a `.tar.gz` named by the
  release's content-derived ID.
- `dev` builds and runs everything locally. Join `localhost:25565` with Minecraft Java Edition 26.2 and the lobby starts
  for you. Add `--offline-logins` to test without Mojang authentication. Press `q` or Ctrl-C to stop.

While `dev` runs, it rebuilds when you save. New players get the new version. Players already in a session stay on the
old one until they leave, or are disconnected after 30 seconds if the gameplay code changed (`--drain-seconds`). Press
`r` to restart everything at once (with `--plain`, type `r` and Enter). Backend data persists in `.chunk/local` between
runs. `clean` deletes `dist/` and `.chunk/` but keeps that data; `clean --data` deletes it too.

## What's here

| Path                                  | Contents                                                                       |
| ------------------------------------- | ------------------------------------------------------------------------------ |
| `apps/lobby/app.ts`                   | The `lobby` app: its stable ID, players per session and its `main` destination |
| `apps/lobby/src/main/`                | Gameplay: the JVM's `main` and the `@SessionType("default")` session provider  |
| `apps/scope.ts`                       | Hooks for every app: the server-list response and routing players to the lobby |
| `server/greetings.ts`                 | The `message` query the lobby calls when a player joins                        |
| `server/schema/index.ts`              | The backend's tables (none yet)                                                |
| `chunk.toml`                          | Local settings: machine profiles, players per session and how many JVMs to run |
| `settings.gradle.kts`, `*.gradle.kts` | Gradle builds, with the chunk plugin and library versions pinned               |

Gameplay code calls backend functions through a generated client that mirrors their paths: the `message` export of
`server/greetings.ts` is `backend.shared().greetings().message(...)` in Java and `backend.shared.greetings.message(...)`
in Kotlin. Add a function or table and the client and types update on the next build.

To add an app, create a directory under `apps/` with an `app.ts` declaring a new `id` and a `build.gradle.kts` like the
lobby's; the build picks it up automatically. Send players to it by returning one of its destinations from the routing
hook, imported from `#chunk/apps`. A `scope.ts` in any directory under `apps/` adds hooks and commands for the apps
below it.

See the [TypeScript SDK](https://github.com/chunkzero/chunk/blob/main/crates/chunk-build/sdk/README.md), the
[session API](https://github.com/chunkzero/chunk/blob/main/jvm/multistom/README.md) and the
[Gradle plugin](https://github.com/chunkzero/chunk/blob/main/jvm/gradle-plugin/README.md) for the details.

## Toolchain

The Gradle wrapper downloads Gradle on the first build, which needs Java installed to start. The build uses Java 25 and
downloads it if needed. Node is not needed: the CLI includes its TypeScript compiler. Keep `gradlew`, `gradlew.bat` and
`gradle/wrapper/` in version control.

Gradle downloads the chunk libraries and plugins from `maven.chunkzero.com`, at the versions pinned in
`settings.gradle.kts` when the CLI created the project. A newer CLI does not change those pins, so keep the CLI and the
pins on the same version.

`./gradlew` and IDE builds run the `chunk` on your `PATH`; pass `-Pchunk.executable=...` to use another. `chunk build`
and `chunk dev` always use the CLI you run them with. To build against unpublished chunk libraries, set the
`chunk.mavenRepository` Gradle property to another Maven repository, or set `chunk.source` to a chunk checkout at the
CLI's revision to build them from source (`chunk create --chunk-source` does this).
