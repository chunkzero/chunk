# Chunk Gradle integration

`dev.chunkzero.chunk.settings` asks an installed `chunk inspect PROJECT` for the
app inventory, then includes each `apps/<id>` Gradle project. Settings evaluation
only reads metadata. Add an `app.toml` and `build.gradle.kts` under a new app
folder to include it on the next build, including with the configuration cache.
The project root requires `chunk.toml`; an empty file is valid.

For a Java project:

```kotlin
// settings.gradle.kts
plugins { id("dev.chunkzero.chunk.settings") version "0.1.0" }
rootProject.name = "my-game"
dependencyResolutionManagement { repositories { mavenCentral() } }
```

```kotlin
// build.gradle.kts and apps/<id>/build.gradle.kts
plugins { id("dev.chunkzero.chunk") }
java { toolchain.languageVersion = JavaLanguageVersion.of(25) }
```

Use your normal Gradle toolchain resolver or an installed JDK. Every project must
choose a Java toolchain explicitly, either in its own build or through a shared
Gradle convention. The selected Minestom runtime requires Java 25 or newer.
Compiler release/target settings must match the selected toolchain. Chunk does
not silently select the Gradle daemon's JVM or replace an app's toolchain.

The root project compiles one `chunk-backend` JAR containing the generated Java
models, references and asynchronous client. Apps depend on that shared JAR and
the Chunk runtime. A Java app has no Kotlin runtime dependency.

Each app supplies one public `SessionProvider` with a no-argument constructor and
`Session create()` method. Add its class name to
`src/main/resources/META-INF/services/dev.chunkzero.runtime.SessionProvider`.
The plugin writes `META-INF/chunk/app.json` into that app's JAR using the discovered
ID. Shared libraries have no app manifest. The runtime validates provider origin
against the app JAR and creates separate session state for each instance.

For Kotlin, put its standard plugin declaration in the settings `plugins` block:

```kotlin
plugins {
    id("org.jetbrains.kotlin.jvm") version "2.4.10" apply false
    id("dev.chunkzero.chunk.settings") version "0.1.0"
}
```

Then apply `dev.chunkzero.chunk.kotlin` in the root build and each Kotlin app.
The settings placement makes Kotlin's API available to the same classloader as
Chunk's settings plugin; declaring it only in a project build is insufficient.
Java apps in a mixed project continue to use `dev.chunkzero.chunk`.

The Kotlin opt in adds the separate `:chunk:backend-kotlin` project and its
`chunk-backend-kotlin` JAR, containing only the generated coroutine facade. It
uses the root's explicit toolchain and depends on the shared Java JAR. Kotlin
apps also receive the coroutine runtime adapters. Java models are compiled once,
even when multiple Java and Kotlin apps share them.

The settings extension has three optional properties:

```kotlin
import dev.chunkzero.gradle.ChunkSettingsExtension

extensions.configure<ChunkSettingsExtension> {
    projectDirectory.set(settingsDir) // default
    executable.set("chunk") // default; an installed CLI or an explicit path
    javaPackage.set("dev.chunkzero.generated") // default
}
```

`-Pchunk.executable=/absolute/path/to/chunk` overrides the configured executable.
The consumer plugin invokes the CLI directly; it does not build Rust tools or
install Node packages. The repository's `just toolchain` builds the development
CLI before the standalone example's settings run.

`generateChunkBackend` calls `chunk gen` once per requested task graph, before
compilation. The compiler owns TypeScript dependency resolution, so this task
always invokes it; unchanged generated content still permits incremental JVM
compilation. Backend outputs go to `.chunk/build/backend`, and JVM source outputs
go to `.chunk/generated/jvm`.

That directory has `java/` models/references, `java-client/` asynchronous clients,
and an optional `kotlin/` facade source root. The plugin wires those roots into
their owning projects; applications depend on compiled shared artifacts.

`chunkArtifacts` builds every discovered app and writes
`.chunk/build/jvm/artifacts.json`. This versioned build descriptor includes each
app JAR and Java requirement, the complete resolved runtime classpath with module
or Gradle project identities, and the selected Java executable. Its file paths
are local build inputs for release assembly. App JARs are listed separately from
the shared classpath. Every discovered app must apply a Chunk project plugin.

`chunk build PROJECT` invokes this root task and publishes a complete release
directory and archive under `PROJECT/dist`. It passes its own executable to
Gradle, so inspection and generation use the same CLI installation. The release
keeps Java requirements and dependency identities while excluding machine-local
file paths and the selected Java executable. `chunk dev PROJECT` uses that
executable locally unless `--java PATH` overrides it.

The [standalone example](../../examples/local/settings.gradle.kts) uses included
builds for the plugin and framework libraries while they are developed together.
Run `./gradlew -p jvm/gradle-plugin test` from the repository root to exercise the
isolated plugin fixtures. `just consumers` builds the real
[Java consumer](../../examples/java/README.md) and Kotlin example from source-only
scratch copies, then checks their complete release archives and runtime classpaths.
It uses the prepared development CLI and starts no Minecraft or backend services.
