# Chunk Gradle integration

`dev.chunkzero.chunk.settings` asks an installed `chunk inspect PROJECT` for the app inventory, then includes each
`apps/<id>` Gradle project. Settings evaluation only reads metadata. Add an `app.toml` and `build.gradle.kts` under a
new app folder to include it on the next build, including with the configuration cache. The project root requires
`chunk.toml`; an empty file is valid.

For a Java project after the SDK version is published (see [distribution](../../docs/distribution.md) for the local
packaged repository):

```kotlin
// settings.gradle.kts
pluginManagement {
    repositories {
        maven("https://maven.chunkzero.com")
        gradlePluginPortal()
        mavenCentral()
    }
}
plugins { id("dev.chunkzero.chunk.settings") version "0.1.0" }
rootProject.name = "my-game"
dependencyResolutionManagement {
    repositories {
        maven("https://maven.chunkzero.com")
        maven("https://maven.chunkzero.com/snapshots") {
            mavenContent { includeModule("net.minestom", "minestom") }
        }
        mavenCentral()
    }
}
```

```kotlin
// build.gradle.kts and apps/<id>/build.gradle.kts
plugins { id("dev.chunkzero.chunk") }
java { toolchain.languageVersion = JavaLanguageVersion.of(25) }
```

Use your normal Gradle toolchain resolver or an installed JDK. Every project must choose a Java toolchain explicitly,
either in its own build or through a shared Gradle convention. The selected Minestom runtime requires Java 25 or newer.
Compiler release/target settings must match the selected toolchain. Chunk does not silently select the Gradle daemon's
JVM or replace an app's toolchain.

The root project compiles one `chunk-backend` JAR containing the generated Java models, references and asynchronous
client. Apps depend on that shared JAR and the Chunk runtime. A Java app has no Kotlin runtime dependency.

Each app supplies public `SessionProvider` factories with no-argument constructors and `Session create()` methods.
Annotate them with `@SessionType("default")`. The plugin validates the compiled factories and generates
`META-INF/services/dev.chunkzero.runtime.SessionProvider` for the JVM's local factory registry, plus a separate session
ID catalog for release assembly. App JARs carry no Chunk deployment manifest. Placement and capacity settings belong in
`app.toml`, with app defaults under `[runtime]` and optional overrides under `[sessions.<id>]`.

## Typed session methods

Declare a method in an app's TypeScript sources and import that authored reference from backend code:

```ts
import { sessionMethod, v } from "#chunk";
export const announce = sessionMethod({
  app: "lobby",
  session: "default",
  name: "announce",
  args: { message: v.string() },
  returns: v.integer(),
});
```

Java or Kotlin gameplay implements the generated single-method interface:

```java
public final class LobbySession extends Session
        implements SessionMethods.Lobby.Default.Announce {
    public Long announce(SessionMethods.Lobby.Default.Announce.Args args) {
        // Update session state synchronously and return a schema value.
        return 0L;
    }
}
```

The provider must return the public concrete class (`LobbySession create()`). The compiler generates argument/result
models and interfaces using the same schema rules as backend clients, including IDs, unions, nullable values and arrays.
JVM-native objects, futures and Kotlin suspend functions cannot implement this synchronous wire signature. Methods from
another app or session type are rejected during indexing.

The build order is backend declarations → shared Java models/interfaces → app Java/Kotlin → bytecode index → generated
method adapters. No generated TypeScript imports or previously compiled JVM classes are needed to bootstrap the build.
`generateChunkSessionRegistry` writes a version-1 `META-INF/chunk/session-methods.json` and local method provider
service; `compileChunkSessionMethods` compiles its direct-call adapters into the app JAR. The local bindings validate
JSON inputs and outputs. Internal control dispatch authenticates captured player membership and process/session
generations before invoking these adapters on the tick thread. The trusted Rust caller API is ready for action and
command integration. The generated provider calls the existing live session instance; `SessionScope` continues to own
its resources and cleanup. No additional component or dependency-injection framework is required for method dispatch.

## Explicit scoped components

When several gameplay objects share dependencies, declare public static factories using
`dev.chunkzero.runtime.Component`. The exact return class identifies a component; parameters identify its dependencies:

```java
public final class LobbyComponents {
    @Component(Component.Scope.SESSION)
    public static BackendClient backend(BackendSession session) {
        return new BackendClient(session);
    }
}
```

Gameplay obtains this client with `scope.component(BackendClient.class)` on the session tick thread. `SESSION` creates
one instance per session; `PROCESS` shares one instance within that app runtime. Session factories may depend on
`SessionScope`, its `BackendSession`, and other declared components. Process factories can depend only on process
components. A missing backend rejects construction when a factory requests it. A factory must receive dependencies
through its parameters; recursively calling `scope.component` from a factory is rejected.

Kotlin uses the same annotation on public top-level functions or `@JvmStatic` factories, including companion objects.
Factories and their parameter/return types must be accessible from generated Java. Generic signatures (including
`List<Foo>` and type variables), primitive/array identities, field injection and constructor discovery are unsupported.
An ordinary non-generic wrapper can give a collection a distinct component identity. Factories may return interfaces,
but binding and lookup use that exact declared interface, without assignability-based selection or qualifiers.

Every module applying the Chunk project plugin writes a bounded factory-class index after Java/Kotlin compilation. Apps
read their dependency JAR indexes, inspect the actual annotated bytecode, reject missing/duplicate providers, cycles and
process-to-session dependencies, then generate direct factory calls. `generateChunkComponentIndex`,
`generateChunkComponentBindings` and `compileChunkComponents` package the index and one app-local service provider.
Libraries containing factories must apply the plugin and declare the framework dependencies they use. Runtime startup
loads that generated provider without scanning classpaths. Shared libraries are linked independently into each app. No
generated source must exist before application compilation, and unused projects emit no component registration.

Returned `AutoCloseable` instances are owned automatically. Failed construction closes only newly created dependencies;
existing components and other sessions remain available. Normal session disposal closes its components in reverse
construction order. Process shutdown closes any remaining component scopes after Minestom stops, then process
components. Cleanup must be synchronous and tolerate shutdown after ticks stop. Component factories must not return an
already owned resource under another identity. These checks enforce declared dependencies and managed ownership;
arbitrary handwritten global state is outside the factory graph's guarantees.

## Kotlin consumers

For Kotlin, put its standard plugin declaration in the settings `plugins` block:

```kotlin
plugins {
    id("org.jetbrains.kotlin.jvm") version "2.4.10" apply false
    id("dev.chunkzero.chunk.settings") version "0.1.0"
}
```

Then apply `dev.chunkzero.chunk.kotlin` in the root build and each Kotlin app. The settings placement makes Kotlin's API
available to the same classloader as Chunk's settings plugin; declaring it only in a project build is insufficient. Java
apps in a mixed project continue to use `dev.chunkzero.chunk`.

The Kotlin opt in adds the separate `:chunk:backend-kotlin` project and its `chunk-backend-kotlin` JAR, containing only
the generated coroutine facade. It uses the root's explicit toolchain and depends on the shared Java JAR. Kotlin apps
also receive the coroutine runtime adapters. Java models are compiled once, even when multiple Java and Kotlin apps
share them.

The settings extension has three optional properties:

```kotlin
import dev.chunkzero.gradle.ChunkSettingsExtension

extensions.configure<ChunkSettingsExtension> {
    projectDirectory.set(settingsDir) // default
    executable.set("chunk") // default; an installed CLI or an explicit path
    javaPackage.set("dev.chunkzero.generated") // default
}
```

`-Pchunk.executable=/absolute/path/to/chunk` overrides the configured executable. The consumer plugin invokes the CLI
directly; it does not build Rust tools or install Node packages. The repository's `just toolchain` builds the
development CLI before the standalone example's settings run.

`generateChunkBackend` calls `chunk gen` once per requested task graph, before compilation. The compiler owns TypeScript
dependency resolution, so this task always invokes it; unchanged generated content still permits incremental JVM
compilation. Backend outputs go to `.chunk/build/backend`, and JVM source outputs go to `.chunk/generated/jvm`.

That directory has `java/` models/references, `java-client/` asynchronous clients, and an optional `kotlin/` facade
source root. The plugin wires those roots into their owning projects; applications depend on compiled shared artifacts.

`chunkArtifacts` builds every discovered app and writes `.chunk/build/jvm/artifacts.json`. The version-4 descriptor
includes each executable app JAR, its session type IDs and Java requirement, plus the selected Java executable. Provider
class names remain in the local service registry. Descriptor file paths are local inputs for release assembly. Every
discovered app must apply a Chunk project plugin. `chunk dev` passes `-Pchunk.dev=true`, which skips the shadow JAR:
each app's descriptor entry names its thin `jar` output and lists its `runtimeClasspath` JARs instead.

`chunk build PROJECT` invokes this root task and publishes a complete release directory and archive under
`PROJECT/dist`. It passes its own executable to Gradle, so inspection and generation use the same CLI installation. The
release keeps Java requirements and resolved deployment settings while excluding machine-local paths and the selected
Java executable. `chunk dev PROJECT` uses that executable locally unless `--java PATH` overrides it.

The [standalone example](../../examples/local/settings.gradle.kts) uses included builds for the plugin and framework
libraries while they are developed together. Run `./gradlew -p jvm/gradle-plugin test` from the repository root to
exercise the isolated plugin fixtures. `just consumers` builds the real [Java consumer](../../examples/java/README.md)
and Kotlin example from source-only scratch copies, then checks their complete release archives and runtime classpaths.
It uses the prepared development CLI and starts no Minecraft or backend services.
