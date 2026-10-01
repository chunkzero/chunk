# Gradle plugin

The Gradle plugins that build a project's apps. They ask the `chunk` CLI which apps the project has, generate the typed
backend bindings before JVM compilation, index each app's session types, session methods and components, and describe
the finished app JARs for `chunk build` to package into a release. This is a separate Kotlin build, included by the
repository's root build.

| Plugin                         | Apply in                        | Does                                                            |
| ------------------------------ | ------------------------------- | --------------------------------------------------------------- |
| `dev.chunkzero.chunk.settings` | `settings.gradle.kts`           | Runs `chunk inspect` and includes every app as a Gradle project |
| `dev.chunkzero.chunk`          | The root build and Java modules | Shared Java bindings, app packaging and indexing                |
| `dev.chunkzero.chunk.kotlin`   | The root build and Kotlin apps  | Everything above, plus Kotlin and the coroutine backend facade  |

## Setup

`chunk create` writes these files, pinned to the CLI's version. By hand, `settings.gradle.kts` looks like this:

```kotlin
pluginManagement {
    repositories {
        maven("https://maven.chunkzero.com")
        gradlePluginPortal()
        mavenCentral()
    }
}

plugins {
    id("org.jetbrains.kotlin.jvm") version "2.4.10" apply false // Kotlin projects only
    id("dev.chunkzero.chunk.settings") version "0.1.0"
    id("org.gradle.toolchains.foojay-resolver-convention") version "1.0.0"
}

dependencyResolutionManagement {
    repositories {
        maven("https://maven.chunkzero.com")
        maven("https://maven.chunkzero.com/snapshots") {
            mavenContent { includeModule("net.minestom", "minestom") }
        }
        mavenCentral()
    }
}

rootProject.name = "my-server"
```

The root `build.gradle.kts` and each app's `build.gradle.kts` apply a project plugin and choose a Java toolchain; apps
also name their main class:

```kotlin
// apps/lobby/build.gradle.kts
plugins { id("dev.chunkzero.chunk") }

java { toolchain.languageVersion = JavaLanguageVersion.of(25) }

application { mainClass = "example.Lobby" }
```

- Every module that applies a Chunk plugin must set its Java toolchain explicitly, to Java 25 or newer (the Minestom
  runtime's requirement), and compile for that same version. `validateChunkJvm` fails the build otherwise; Chunk never
  falls back to the Gradle daemon's JVM.
- The root applies the same plugin as the apps before they do. For Kotlin apps that is `dev.chunkzero.chunk.kotlin`, and
  the Kotlin Gradle plugin's version must be declared in the settings `plugins` block, as above, so that Chunk's
  settings plugin can see it. Java apps in a Kotlin project keep `dev.chunkzero.chunk`.
- A library module (such as a `:shared` project with common gameplay code) applies a Chunk plugin too if it declares
  `@Component` factories, so its factories are indexed.

The project plugins add the Chunk libraries at the plugin's own version: the root exports
`dev.chunkzero:backend-client`, Java apps get `runtime-minestom` and Kotlin apps `runtime-minestom-kotlin`.

### Settings

The settings plugin adds a `chunk` extension:

```kotlin
import dev.chunkzero.gradle.ChunkSettingsExtension

extensions.configure<ChunkSettingsExtension> {
    projectDirectory.set(settingsDir) // default; the directory with chunk.toml
    executable.set("chunk") // default; a CLI on PATH or an absolute path
    javaPackage.set("dev.chunkzero.generated") // default; the package of generated bindings
}
```

The `chunk.executable` Gradle property overrides `executable`. `chunk create` records the creating CLI in the project's
`gradle.properties`, and `chunk build` and `chunk dev` always pass the CLI they run as. The plugin runs that CLI; it
never builds Rust tools or installs Node packages.

## What a build does

1. Settings evaluation runs `chunk inspect` and includes each app directory as a Gradle project: `apps/games/arena`
   becomes `:apps:games:arena`. Adding an `app.ts` and a `build.gradle.kts` under `apps/` adds an app on the next build,
   including with the configuration cache. It also includes a reserved `:chunk:backend-kotlin` project under
   `.chunk/gradle/`.
2. `generateChunkBackend`, on the root, runs `chunk gen` before any JVM compilation. The compiled backend goes to
   `.chunk/build/backend` and JVM sources to `.chunk/generated/jvm`: `java/` (models, references, `SessionMethods`,
   `SessionConfigs`, `Destinations`), `java-client/` (`BackendClient`), `java-session/<app>/` (configured provider
   interfaces) and, for Kotlin, `kotlin/` (`CoroutineBackendClient`). The task always runs, since the compiler resolves
   TypeScript dependencies itself; unchanged output keeps JVM compilation incremental.
3. The root compiles `java/` and `java-client/` once into the shared `chunk-backend` JAR. With the Kotlin plugin,
   `:chunk:backend-kotlin` compiles the coroutine facade into `chunk-backend-kotlin`. Apps depend on these JARs, so a
   Java app has no Kotlin dependency.
4. Each app applies `application` and Shadow, compiles, and is indexed from its bytecode (below). Its `shadowJar` is an
   executable JAR with every dependency.
5. `chunkArtifacts`, on the root, builds every app and writes `.chunk/build/jvm/artifacts.json`: each app's JAR, session
   types and Java version, and the Java executable of the newest toolchain. `chunk build` runs this task and packages
   the result into a release. `chunk dev` passes `-Pchunk.dev=true`, which skips the shadow JAR and lists each app's
   thin JAR and runtime classpath instead.

## Session types

The app's compiled classes are scanned for `@SessionType("id")`. Each annotated class must be public, concrete, have a
public no-argument constructor and implement `SessionProvider`; an app has 1 to 128 of them, with distinct IDs, and its
main class needs a `public static void main(String[])`. `generateChunkSessionRegistry` writes
`META-INF/services/dev.chunkzero.runtime.SessionProvider`, from which the runtime loads the providers, and a catalog of
the session type IDs for release assembly. Packaging fails unless those IDs exactly match the app's `implementations` in
`app.ts` (by default just `default`).

An implementation with a `config` validator must implement its generated interface, for example
`LobbySessionProviders.Default`, and receives the validated configuration in `create(SessionCreation<Config>)`; one
without must implement plain `SessionProvider`.

## Session methods

For each `sessionMethod` declared in TypeScript (see the [SDK](../../crates/chunk-build/sdk/README.md#session-methods)),
the generated `SessionMethods` class has a single-method interface with `Args` and result types. The session class
implements it, and the provider's `create` must declare that concrete class as its return type. From the
[arena example](../../examples/arena/apps/arena/src/main/java/example/arena/Arena.java):

```java
@SessionType("koth")
public final class Arena implements ArenaSessionProviders.Koth {
    @Override
    public ArenaSession create(SessionCreation<SessionConfigs.Arena.Koth.Config> creation) {
        var config = creation.config();
        return new ArenaSession(
                new Match.Rules(
                        Math.toIntExact(config.targetScore()),
                        Math.toIntExact(config.timeLimitSeconds())));
    }
}

public final class ArenaSession extends Session implements SessionMethods.Arena.Koth.Status {
    @Override
    public String status(SessionMethods.Arena.Koth.Status.Args args) {
        // Runs synchronously on the session's tick thread.
        return "Red %d - %d Blue".formatted(match.score(Team.RED), match.score(Team.BLUE));
    }
}
```

The signature is synchronous: futures and Kotlin `suspend` functions cannot implement it. Implementing a method that
belongs to another app or session type is a build error. `generateChunkSessionRegistry` checks the implementations and
writes `META-INF/chunk/session-methods.json` and a method provider service; `compileChunkSessionMethods` compiles
direct-call adapters into the app, which the runtime loads without scanning the classpath.

## Components

Every module applying a Chunk plugin indexes its public static `@Component` factories after compilation
(`generateChunkComponentIndex`). Each app then reads its own index and those in its dependencies' JARs, checks the graph
for missing or duplicate providers, cycles and process components that depend on session ones
(`generateChunkComponentBindings`), and compiles one provider that calls the factories directly
(`compileChunkComponents`). See the [Minestom runtime](../runtime-minestom/README.md#components) for writing components.

## Testing

`./gradlew -p jvm/gradle-plugin test`, from the repository root, runs Gradle TestKit fixtures against a copy of the
plugin published to a build-local repository. `just consumers` builds the
[arena example](../../examples/arena/README.md) and the [Kotlin example](../../examples/local/README.md) from
source-only scratch copies with the development CLI, then checks their release archives, session registries and
dependency boundaries without starting any services.
