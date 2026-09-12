# My Chunk server

A lobby app with Java or Kotlin gameplay and a typed TypeScript backend greeting.

Run these commands from this directory, using the Chunk CLI prepared with `just toolchain`:

```sh
chunk codegen
chunk build
chunk dev
```

`codegen` prepares the TypeScript SDK for your editor. `build` generates the JVM clients and packages an immutable
release under `dist/`. `dev` builds the release and starts the local backend, control and proxy. Connect with an
official Minecraft Java Edition 26.1 client at `localhost:25565`; the lobby starts automatically on join. Ctrl-C stops
the services and their gameplay JVMs. Local backend data stays in `.chunk/local` between runs. Restart `dev` to apply
code changes; automatic reload is not implemented yet.

The Gradle wrapper selects Java 25, downloading that toolchain when needed. A Java installation is required to start
Gradle. Third-party dependencies also need network access on the first build. No Node installation is needed by this
project: the prepared Chunk CLI includes its TypeScript toolchain.

This starter currently uses a local Chunk source checkout for the Gradle plugin and runtime libraries.
`gradle.properties` records that checkout as `chunk.source` and the CLI as `chunk.executable`. Update these paths when
moving the project to another machine, or override them with Gradle's `-Pchunk.source=...` and `-Pchunk.executable=...`
properties. Use a CLI and source checkout from the same revision. `chunk build` and `chunk dev` automatically use the
CLI executable you invoke.

Gameplay lives in `apps/lobby/src/main/`. Each `SessionProvider` creates a fresh session; `@SessionType("default")`
registers it as `lobby/default`. Edit `server/greetings.ts` to change the typed greeting. `server/proxy.ts` owns
admission and routing. The root `server/schema/index.ts` explicitly composes backend tables; it starts empty. Add apps
as immediate `apps/NAME/` directories with `app.toml` and `build.gradle.kts`; the settings plugin discovers them
automatically.
