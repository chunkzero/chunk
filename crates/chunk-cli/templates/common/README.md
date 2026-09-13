# My Chunk server

A lobby app with Java or Kotlin gameplay and a typed TypeScript backend greeting.

Run these commands from this directory:

```sh
@CHUNK_COMMAND@ codegen
@CHUNK_COMMAND@ build
@CHUNK_COMMAND@ dev
```

`codegen` prepares the TypeScript SDK for your editor. `build` generates the JVM clients and packages an immutable
release under `dist/`. `dev` builds the release and starts the local backend, control and proxy. Connect with an
official Minecraft Java Edition 26.1 client at `localhost:25565`; the lobby starts automatically on join. Ctrl-C stops
the services and their gameplay JVMs. Local backend data stays in `.chunk/local` between runs. Restart `dev` to apply
code changes; automatic reload is not implemented yet.

The Gradle wrapper selects Java 25, downloading that toolchain when needed. A Java installation is required to start
Gradle. Third-party dependencies also need network access on the first build. No Node installation is needed by this
project: the prepared Chunk CLI includes its TypeScript toolchain.

Gradle downloads the pinned Chunk JVM libraries and plugins from `maven.chunkzero.com`. The version pins in
`settings.gradle.kts` come from the SDK that created this project. Installing a new CLI does not upgrade those pins; use
a matching CLI and JVM SDK version.

You can replace the absolute CLI path above with `chunk` once it is on your `PATH`. `gradle.properties` records
`chunk.executable` for direct `./gradlew` runs and IDE builds; update that path when moving machines, or override it
with `-Pchunk.executable=...`. `chunk build` and `chunk dev` use the CLI executable you invoke.

For framework development, `chunk create --chunk-source CHECKOUT` records an explicit `chunk.source` override and builds
the JVM libraries from that checkout. Use the CLI and checkout from the same revision. An alternate Maven repository for
an unpublished SDK can be configured with the `chunk.mavenRepository` Gradle property.

Gameplay lives in `apps/lobby/src/main/`. Each `SessionProvider` creates a fresh session; `@SessionType("default")`
registers it as `lobby/default`. Edit `server/greetings.ts` to change the typed greeting. `server/proxy.ts` owns
admission and routing. The root `server/schema/index.ts` explicitly composes backend tables; it starts empty. Add apps
as immediate `apps/NAME/` directories with `app.toml` and `build.gradle.kts`; the settings plugin discovers them
automatically.
