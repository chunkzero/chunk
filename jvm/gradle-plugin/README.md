# Gradle plugin

The Gradle plugins that build a project's apps. They ask the `chunk` CLI which apps the project has, generate the typed
backend bindings before JVM compilation, index each app's session types, session methods and components, and describe
the finished app JARs for `chunk build` to package into a release. This is a separate Kotlin build, included by the
repository's root build.

| Plugin                         | Apply in                        | Does                                                            |
| ------------------------------ | ------------------------------- | --------------------------------------------------------------- |
| `com.chunkzero.chunk.settings` | `settings.gradle.kts`           | Runs `chunk inspect` and includes every app as a Gradle project |
| `com.chunkzero.chunk`          | The root build and Java modules | Shared Java bindings, app packaging and indexing                |
| `com.chunkzero.chunk.kotlin`   | The root build and Kotlin apps  | Everything above, plus Kotlin and the coroutine backend facade  |

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
    id("com.chunkzero.chunk.settings") version "0.1.0"
    id("org.gradle.toolchains.foojay-resolver-convention") version "1.0.0"
}

dependencyResolutionManagement {
    repositories {
        maven("https://maven.chunkzero.com")
        maven("https://maven.chunkzero.com/nightlies") {
            mavenContent { includeGroup("com.chunkzero.multistom") }
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
plugins { id("com.chunkzero.chunk") }

java { toolchain.languageVersion = JavaLanguageVersion.of(25) }

application { mainClass = "example.Lobby" }
```

- Every module that applies a Chunk plugin must set its Java toolchain explicitly, to Java 25 or newer (the Minestom
  runtime's requirement), and compile for that same version. `validateChunkJvm` fails the build otherwise; Chunk never
  falls back to the Gradle daemon's JVM.
- The root applies the same plugin as the apps before they do. For Kotlin apps that is `com.chunkzero.chunk.kotlin`, and
  the Kotlin Gradle plugin's version must be declared in the settings `plugins` block, as above, so that Chunk's
  settings plugin can see it. Java apps in a Kotlin project keep `com.chunkzero.chunk`.
- A library module (such as a `:shared` project with common gameplay code) applies a Chunk plugin too if it declares
  `@Component` factories, so its factories are indexed.

The project plugins add the Chunk libraries at the plugin's own version: the root exports
`com.chunkzero.chunk:backend-client`, Java apps get `multistom` and Kotlin apps `multistom-kotlin`.

### Settings

The settings plugin adds a `chunk` extension:

```kotlin
import com.chunkzero.chunk.gradle.ChunkSettingsExtension

extensions.configure<ChunkSettingsExtension> {
    projectDirectory.set(settingsDir) // default; the directory with chunk.toml
    executable.set("chunk") // default; a CLI on PATH or an absolute path
    javaPackage.set("com.chunkzero.chunk.generated") // default; the package of generated bindings
}
```

The `chunk.executable` Gradle property overrides `executable`; `chunk build` and `chunk dev` always pass the CLI they
run as. The plugin runs that CLI; it never builds Rust tools or installs Node packages.

## What a build does

1. Settings evaluation runs `chunk inspect` and includes each app directory as a Gradle project: `apps/games/arena`
   becomes `:apps:games:arena`. Adding an `app.ts` and a `build.gradle.kts` under `apps/` adds an app on the next build,
   including with the configuration cache. It also includes a reserved `:chunk:backend-kotlin` project under
   `.chunk/gradle/`.
2. `generateChunkBackend`, on the root, runs `chunk gen` before any JVM compilation. The compiled backend goes to
   `.chunk/build/backend` and JVM sources to `.chunk/generated/jvm`: `java/` (models, references, `SessionMethods`,
   `SessionConfigs`, `Destinations`, `Vars`), `java-client/` (`BackendClient`), `java-session/<app>/` (configured
   provider interfaces) and, for Kotlin, `kotlin/` (`CoroutineBackendClient`). The task always runs, since the compiler
   resolves TypeScript dependencies itself; unchanged output keeps JVM compilation incremental.
3. The root compiles `java/` and `java-client/` once into the shared `chunk-backend` JAR. With the Kotlin plugin,
   `:chunk:backend-kotlin` compiles the coroutine facade into `chunk-backend-kotlin`. Apps depend on these JARs, so a
   Java app has no Kotlin dependency.
4. Each app applies `application` and Shadow, compiles, and is indexed from its bytecode (below). Its `shadowJar` is an
   executable JAR with every dependency.
5. Each app's Anvil worlds (`format: "anvil"` in `chunk inspect`) are converted to
   `.chunk/build/worlds/<app>/<name>.polar` by `convertChunkWorld_<name>`, cropped to the world's `chunks` if given. The
   converter runs in the app's Java toolchain with the Minestom and Polar of its runtime classpath, leaving out the
   build's own projects so code changes don't reconvert. It rejects a save whose data version isn't the runtime's
   Minecraft version. The conversion is cacheable and its output is the same bytes for the same save.
6. `chunkArtifacts`, on the root, builds every app and writes `.chunk/build/jvm/artifacts.json`: each app's JAR, session
   types and Java version, and the Java executable of the newest toolchain. `chunk build` runs this task and packages
   the result into a release. `chunk dev` passes `-Pchunk.dev=true`, which skips the shadow JAR and lists each app's
   thin JAR and runtime classpath instead.

## Session types

The app's compiled classes are scanned for `@SessionType("id")`. Each annotated class must be public, concrete, have a
public no-argument constructor and implement `com.chunkzero.chunk.runtime.SessionProvider`; an app has 1 to 128 of them,
with distinct IDs, and its main class needs a `public static void main(String[])`. `generateChunkSessionRegistry` writes
`META-INF/services/com.chunkzero.chunk.runtime.SessionProvider`, from which the runtime loads the providers, and a
catalog of the session type IDs for release assembly. Packaging fails unless those IDs exactly match the app's
`implementations` in `app.ts` (by default just `default`).

An implementation with a `config` validator must implement its generated interface, for example
`LobbySessionProviders.Default<LobbySession>`, and receives the validated configuration in
`create(SessionCreation<Config>)`; one without must implement plain `SessionProvider<LobbySession>`. The type argument
is the session class; the plugin only checks the contracts, while the engine decides which session classes it accepts.

## Session methods

For each `sessionMethod` declared in TypeScript (see the [SDK](../../crates/chunk-build/sdk/README.md#session-methods)),
the generated `SessionMethods` class has a single-method interface with `Args` and result types. The session class
implements it, and the provider's `create` must declare that public concrete class as its return type. From the
[arena example](../../examples/arena/apps/arena/src/main/java/example/arena/Arena.java):

```java
@SessionType("koth")
public final class Arena implements ArenaSessionProviders.Koth<ArenaSession> {
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
writes `META-INF/chunk/session-methods.json` and a `com.chunkzero.chunk.runtime.SessionMethodProvider` service;
`compileChunkSessionMethods` compiles typed direct-call adapters into the app, such as
`new SessionMethodBinding<>(Status.REF, ArenaSession.class, (session, args) -> session.status(args))`, which the runtime
loads without scanning the classpath.

## Components

Every module applying a Chunk plugin indexes its public static `@Component` factories after compilation
(`generateChunkComponentIndex`). Each app then reads its own index and those in its dependencies' JARs, checks the graph
for missing or duplicate providers, cycles and process components that depend on session ones
(`generateChunkComponentBindings`), and compiles one `com.chunkzero.chunk.runtime.ComponentProvider` that calls the
factories directly (`compileChunkComponents`). See the [Minestom runtime](../multistom/README.md#components) for writing
components.

The host supplies some types to session-scoped factories itself: `BackendSession`, and any class on the app's runtime
classpath annotated `@Component.Supplied`, such as an engine's session handle. A factory may take them only if it is
session-scoped, and no factory may provide them.

## Testing

`./gradlew -p jvm/gradle-plugin test`, from the repository root, runs Gradle TestKit fixtures against a copy of the
plugin published to a build-local repository. `just consumers` builds the
[arena example](../../examples/arena/README.md) and the [Kotlin example](../../examples/local/README.md) from
source-only scratch copies with the development CLI, then checks their release archives, session registries and
dependency boundaries without starting any services.
