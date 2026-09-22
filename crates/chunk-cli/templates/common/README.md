# @PROJECT_NAME@

A lobby app with Java or Kotlin gameplay and a typed TypeScript backend greeting.

Run these commands from this directory:

```sh
@CHUNK_COMMAND@ codegen
@CHUNK_COMMAND@ build
@CHUNK_COMMAND@ dev
```

`codegen` prepares the TypeScript SDK in `.chunk/sdk/` and typed bindings in `.chunk/generated/` for your editor. Gradle
build support is generated in `.chunk/gradle/`. These directories are ignored by Git and recreated when needed. `build`
generates the JVM clients and packages an immutable release under `dist/`. `dev` builds the release and starts the local
backend, control and proxy. Connect with an official Minecraft Java Edition 26.2 client at `localhost:25565`; the lobby
starts automatically on join. Ctrl-C stops the services and their gameplay JVMs. Local backend data stays in
`.chunk/local` between runs. `dev` rebuilds when sources change and sends new players to the new version; press `r`
(or type `r` and Enter with `--plain`) to restart every session immediately.

The standard Gradle wrapper downloads and caches Gradle on the first build. The build selects Java 25, downloading that
toolchain when needed. A Java installation is required to start Gradle. Keep `gradlew`, `gradlew.bat` and
`gradle/wrapper/` in version control. Third-party dependencies also need network access on the first build. No Node
installation is needed by this project: the prepared Chunk CLI includes its TypeScript toolchain.

Gradle downloads the pinned Chunk JVM libraries and plugins from `maven.chunkzero.com`. The version pins in
`settings.gradle.kts` come from the CLI that created this project. Installing a new CLI does not upgrade those pins; use
a matching CLI and JVM SDK version.

You can replace the absolute CLI path above with `chunk` once it is on your `PATH`. `gradle.properties` records
`chunk.executable` for direct `./gradlew` runs and IDE builds; update that path when moving machines, or override it
with `-Pchunk.executable=...`. `chunk build` and `chunk dev` use the CLI executable you invoke.

For framework development, `chunk create --chunk-source CHECKOUT` records an explicit `chunk.source` override and builds
the JVM libraries from that checkout. Use the CLI and checkout from the same revision. An alternate Maven repository for
an unpublished SDK can be configured with the `chunk.mavenRepository` Gradle property.

Gameplay lives in `apps/lobby/src/main/`. Each `SessionProvider` creates a fresh session; `@SessionType("default")`
registers it as `lobby/default`. Edit `server/greetings.ts` to change the typed greeting. `apps/scope.ts` owns inherited
server policy and initial routing. `apps/lobby/app.ts` declares the stable app ID, capacity, destinations and optional
app-local hooks/commands. Import generated references from `#chunk/apps` when selecting a destination. The root
`server/schema/index.ts` explicitly composes backend tables; it starts empty. Add apps anywhere under `apps/` with an
`app.ts` and `build.gradle.kts`; ancestor `scope.ts` files supply inherited policy. The settings plugin discovers the
Gradle projects automatically, independently of the explicit app IDs.
