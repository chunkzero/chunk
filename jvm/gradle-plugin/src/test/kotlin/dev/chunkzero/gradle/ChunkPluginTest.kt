package dev.chunkzero.gradle

import com.google.gson.JsonObject
import com.google.gson.JsonParser
import org.gradle.testkit.runner.GradleRunner
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import org.junit.jupiter.api.io.TempDir
import java.io.File
import java.nio.file.Path
import java.util.jar.JarFile
import java.util.jar.JarOutputStream

class ChunkPluginTest {
    @TempDir
    lateinit var directory: Path

    @Test
    fun `builds independent executable apps with generated catalogs and configuration cache`() {
        fixture()
        app("lobby")
        library("fixture", "shared", "1.0")
        library("fixture", "shared", "2.0")
        directory.resolve("build.gradle.kts").toFile().appendText(
            "\ndependencies { api(\"fixture:shared:1.0\") }\n",
        )
        directory.resolve("apps/lobby/build.gradle.kts").toFile().appendText(
            "\ndependencies { implementation(\"fixture:shared:2.0\") }\n",
        )
        val projects = run("projects")
        assertTrue(projects.output.contains(":apps:lobby"))
        assertEquals(listOf("inspect"), calls())
        run("chunkArtifacts")
        val first = descriptor()
        assertSessionRegistries(first)
        assertEquals(listOf("lobby"), first.getAsJsonArray("apps").map { it.asJsonObject["id"].asString })
        assertFalse(first.toString().contains("kotlin"))
        assertFalse(first.has("classpath"))
        JarFile(first.getAsJsonArray("apps")[0].asJsonObject["jar"].asString).use {
            assertTrue(it.getEntry("fixture/generated/Bindings.class") != null)
            assertEquals(
                "2.0",
                it.getInputStream(it.getEntry("fixture/shared-version.txt")).bufferedReader().readText(),
            )
        }
        JarFile(directory.resolve("build/libs/chunk-backend.jar").toFile()).use {
            assertTrue(it.getEntry("fixture/generated/Bindings.class") != null)
            assertTrue(it.getEntry("META-INF/chunk/app.json") == null)
        }
        val reused = run("chunkArtifacts")
        assertTrue(reused.output.contains("Reusing configuration cache"))
        assertEquals(2, calls().count { it == "gen java" })
        app("arena")
        run("chunkArtifacts")
        assertSessionRegistries(descriptor())
        assertEquals(
            listOf("arena", "lobby"),
            descriptor().getAsJsonArray("apps").map { it.asJsonObject["id"].asString },
        )
        descriptor().getAsJsonArray("apps").forEach { app ->
            JarFile(app.asJsonObject["jar"].asString).use { jar ->
                val expected = if (app.asJsonObject["id"].asString == "lobby") "2.0" else "1.0"
                assertEquals(
                    expected,
                    jar.getInputStream(jar.getEntry("fixture/shared-version.txt")).bufferedReader().readText(),
                )
            }
        }
    }

    @Test
    fun `Kotlin opt in builds a facade separately from shared Java classes`() {
        fixture(kotlin = true)
        app("lobby", kotlin = true)
        run("chunkArtifacts")
        assertSessionRegistries(descriptor())
        val executable = descriptor().getAsJsonArray("apps")[0].asJsonObject["jar"].asString
        JarFile(executable).use {
            assertTrue(it.getEntry("fixture/generated/FacadeKt.class") != null)
            assertTrue(it.getEntry("fixture/generated/Bindings.class") != null)
        }
        assertTrue(calls().contains("gen kotlin"))
        val reused = run("chunkArtifacts")
        assertTrue(reused.output.contains("Reusing configuration cache"), reused.output)
    }

    @Test
    fun `requires explicit compatible toolchains and matching compiler targets`() {
        fixture()
        app("lobby", toolchain = "")
        assertTrue(
            runFailure(":apps:lobby:validateChunkJvm").output.contains("must explicitly configure java.toolchain"),
        )
        app("lobby", toolchain = "java { toolchain.languageVersion = JavaLanguageVersion.of(21) }")
        assertTrue(runFailure(":apps:lobby:validateChunkJvm").output.contains("requires JDK 25 or newer"))
        app("lobby")
        directory.resolve("apps/lobby/build.gradle.kts").toFile().appendText(
            "\ntasks.withType<JavaCompile>().configureEach { options.release = 21 }\n",
        )
        assertTrue(
            runFailure(
                ":apps:lobby:validateChunkJvm",
            ).output.contains("targets Java 21 but the selected toolchain is 25"),
        )
    }

    @Test
    fun `session registry ignores deployment settings and rejects duplicate IDs`() {
        fixture()
        app("lobby")
        run("chunkArtifacts")
        val executable =
            directory.fileSystem
                .getPath(
                    descriptor().getAsJsonArray("apps")[0].asJsonObject["jar"].asString,
                ).toFile()
        val before = executable.readBytes().toList()
        write("apps/lobby/app.toml", "[sessions.default]\nmachine_profile = 'large'\ncapacity = 32")
        run("chunkArtifacts")
        assertEquals(before, executable.readBytes().toList())
        assertSessionRegistries(descriptor())
        write(
            "apps/lobby/src/main/java/Duplicate.java",
            """
            package fixture.lobby;
            @dev.chunkzero.runtime.SessionType("default")
            public final class Duplicate implements dev.chunkzero.runtime.SessionProvider {}
        """,
        )
        assertTrue(runFailure(":apps:lobby:generateChunkSessionRegistry").output.contains("Duplicate session type"))
    }

    @Test
    fun `Java methods compile after clean prerequisites and reject mismatched session contracts`() {
        methodFixture(kotlin = false)
        run("chunkArtifacts")
        assertMethodExecution()
        write(
            "apps/lobby/src/main/java/Game.java",
            """
            package fixture.lobby;
            public final class Game extends dev.chunkzero.runtime.Session {}
        """,
        )
        assertTrue(runFailure("chunkArtifacts").output.contains("must implement fixture.generated.Ping"))
    }

    @Test
    fun `Kotlin implements generated Java method contracts from a clean build`() {
        methodFixture(kotlin = true)
        run("chunkArtifacts")
        assertMethodExecution()
        assertTrue(run("chunkArtifacts").output.contains("Reusing configuration cache"))
    }

    @Test
    fun `Java configured providers compile from app sources and require the exact declared interface`() {
        configurationFixture(kotlin = false)
        run("chunkArtifacts")
        assertMethodExecution()
        val jar = descriptor().getAsJsonArray("apps")[0].asJsonObject["jar"].asString
        JarFile(jar).use {
            val metadata =
                JsonParser
                    .parseString(
                        it
                            .getInputStream(
                                it.getEntry("META-INF/chunk/session-configurations.json"),
                            ).bufferedReader()
                            .readText(),
                    ).asJsonObject
            assertEquals(
                "fixture.lobby.App",
                metadata.getAsJsonArray("configurations")[0].asJsonObject["provider"].asString,
            )
        }
        val source = directory.resolve("apps/lobby/src/main/java/App.java").toFile()
        source.writeText(
            source
                .readText()
                .replace(
                    "fixture.generated.LobbySessionProviders.Default",
                    "dev.chunkzero.runtime.ConfiguredSessionProvider<fixture.generated.Bindings>",
                ).replace(
                    "public Game create",
                    "public dev.chunkzero.backend.api.JsonType<fixture.generated.Bindings> configurationType() { return null; } public Game create",
                ),
        )
        assertTrue(
            runFailure(
                "chunkArtifacts",
            ).output.contains("must implement fixture.generated.LobbySessionProviders.Default"),
        )
    }

    @Test
    fun `Kotlin configured providers retain concrete session methods and immutable typed creation input`() {
        configurationFixture(kotlin = true)
        run("chunkArtifacts")
        assertMethodExecution()
    }

    @Test
    fun `Java components link shared module indexes after clean compilation and reject duplicate identities`() {
        componentFixture(kotlin = false)
        run("chunkArtifacts")
        assertComponentExecution()
        assertTrue(run("chunkArtifacts").output.contains("Reusing configuration cache"))
        write(
            "apps/lobby/src/main/java/Duplicate.java",
            """
            package fixture.lobby;
            public final class Duplicate {
                @dev.chunkzero.runtime.Component(dev.chunkzero.runtime.Component.Scope.PROCESS)
                public static fixture.generated.Bindings duplicate() { return new fixture.generated.Bindings("other"); }
            }
            """,
        )
        assertTrue(runFailure("chunkArtifacts").output.contains("Duplicate component identity"))
        write(
            "apps/lobby/src/main/java/Duplicate.java",
            """
            package fixture.lobby;
            public final class Duplicate {
                private static final class Hidden {
                    public static final class Factories {
                        @dev.chunkzero.runtime.Component(dev.chunkzero.runtime.Component.Scope.PROCESS)
                        public static java.time.Clock clock() { return java.time.Clock.systemUTC(); }
                    }
                }
            }
            """,
        )
        assertTrue(runFailure("chunkArtifacts").output.contains("private access"))
    }

    @Test
    fun `Kotlin top level and companion component factories generate real Java calls and reject erased generics`() {
        componentFixture(kotlin = true)
        run("chunkArtifacts")
        assertComponentExecution()
        assertTrue(run("chunkArtifacts").output.contains("Reusing configuration cache"))
        write(
            "apps/lobby/src/main/kotlin/Generic.kt",
            """
            package fixture.lobby
            @dev.chunkzero.runtime.Component(dev.chunkzero.runtime.Component.Scope.SESSION)
            fun generic(): List<String> = emptyList()
            """,
        )
        assertTrue(runFailure("chunkArtifacts").output.contains("must be public static and non-generic"))
    }

    private fun componentFixture(kotlin: Boolean) {
        fixture(kotlin)
        app("lobby", kotlin)
        val repository = File(System.getProperty("chunk.test.repository"))
        for ((module, name) in listOf(
            "runtime" to "Component",
            "runtime-minestom" to "ComponentBinding",
            "runtime-minestom" to "ComponentProvider",
        )) {
            write(
                "src/main/java/dev/chunkzero/runtime/$name.java",
                repository.resolve("jvm/$module/src/main/java/dev/chunkzero/runtime/$name.java").readText(),
            )
        }
        write(
            "src/main/java/org/jetbrains/annotations/ApiStatus.java",
            "package org.jetbrains.annotations; public @interface ApiStatus { @interface Internal {} }",
        )
        write(
            "src/main/java/SharedServices.java",
            """
            package fixture;
            public final class SharedServices {
                @dev.chunkzero.runtime.Component(dev.chunkzero.runtime.Component.Scope.PROCESS)
                public static fixture.generated.Bindings backendValue() { return new fixture.generated.Bindings("shared"); }
            }
            """,
        )
        write(
            "apps/lobby/src/main/java/Verify.java",
            """
            package fixture.lobby;
            public final class Verify {
                private static java.util.Map<Class<?>, dev.chunkzero.runtime.ComponentBinding<?>> bindings;
                public static void run() throws Exception {
                    bindings = new java.util.HashMap<>();
                    var providers = java.util.ServiceLoader.load(dev.chunkzero.runtime.ComponentProvider.class).stream().toList();
                    if (providers.size() != 1) throw new AssertionError("one generated app registry");
                    for (var binding : providers.getFirst().get().components()) bindings.put(binding.type(),binding);
                    if (bindings.size() != 3) throw new AssertionError("expected shared and app factories");
                    System.out.println(((View) create(View.class)).value());
                }
                private static Object create(Class<?> type) throws Exception {
                    var binding = bindings.get(type);
                    var dependencies = new Object[binding.dependencies().size()];
                    for (int i=0; i<dependencies.length; i++) dependencies[i] = create(binding.dependencies().get(i));
                    return binding.factory().create(dependencies);
                }
            }
            """,
        )
        write("apps/lobby/src/main/java/View.java", "package fixture.lobby; public record View(String value) {}")
        write("apps/lobby/src/main/java/Extra.java", "package fixture.lobby; public record Extra(String value) {}")
        if (kotlin) {
            write(
                "apps/lobby/src/main/kotlin/App.kt",
                """
                package fixture.lobby
                import dev.chunkzero.runtime.Component
                @dev.chunkzero.runtime.SessionType("default")
                class Factory : dev.chunkzero.runtime.SessionProvider
                class Services {
                    companion object {
                        @JvmStatic
                        @Component(Component.Scope.PROCESS)
                        fun extra() = Extra("app")
                    }
                }
                @Component(Component.Scope.SESSION)
                fun view(value: fixture.generated.Bindings, extra: Extra) = View(value.value() + ":" + extra.value())
                fun main() { Verify.run() }
                """,
            )
        } else {
            write(
                "apps/lobby/src/main/java/App.java",
                """
                package fixture.lobby;
                import dev.chunkzero.runtime.Component;
                @dev.chunkzero.runtime.SessionType("default")
                public final class App implements dev.chunkzero.runtime.SessionProvider {
                    @Component(Component.Scope.PROCESS)
                    public static Extra extra() { return new Extra("app"); }
                    @Component(Component.Scope.SESSION)
                    public static View view(fixture.generated.Bindings value, Extra extra) { return new View(value.value() + ":" + extra.value()); }
                    public static void main(String[] args) throws Exception { Verify.run(); }
                }
                """,
            )
        }
    }

    private fun assertComponentExecution() {
        val jar = descriptor().getAsJsonArray("apps")[0].asJsonObject["jar"].asString
        val process =
            ProcessBuilder("${System.getProperty("chunk.test.java.home")}/bin/java", "-jar", jar)
                .redirectErrorStream(true)
                .start()
        val output = process.inputStream.bufferedReader().use { it.readText() }
        assertEquals(0, process.waitFor(), output)
        assertEquals("shared:app", output.trim())
    }

    private fun configurationFixture(kotlin: Boolean) {
        methodFixture(kotlin)
        write("configuration-fixture", "")
        val repository = File(System.getProperty("chunk.test.repository"))
        for (name in listOf("SessionCreation", "ConfiguredSessionProvider", "SessionProvider")) {
            write(
                "apps/lobby/src/main/java/dev/chunkzero/runtime/$name.java",
                repository.resolve("jvm/runtime-minestom/src/main/java/dev/chunkzero/runtime/$name.java").readText(),
            )
        }
        write(
            "src/main/java/dev/chunkzero/backend/api/JsonType.java",
            "package dev.chunkzero.backend.api; public final class JsonType<T> {}",
        )
        if (kotlin) {
            write(
                "apps/lobby/src/main/kotlin/App.kt",
                """
                package fixture.lobby
                import dev.chunkzero.runtime.SessionCreation
                import fixture.generated.Bindings
                @dev.chunkzero.runtime.SessionType("default")
                class Factory : fixture.generated.LobbySessionProviders.Default {
                    override fun create(creation: SessionCreation<Bindings>) = Game()
                }
                class Game : dev.chunkzero.runtime.Session(), fixture.generated.Ping {
                    override fun ping(args: Bindings): String = "echo:" + args.value()
                }
                fun main() { Verify.run(Factory().create(SessionCreation(32, Bindings("ok")))) }
                """,
            )
        } else {
            write(
                "apps/lobby/src/main/java/App.java",
                """
                package fixture.lobby;
                @dev.chunkzero.runtime.SessionType("default")
                public final class App implements fixture.generated.LobbySessionProviders.Default {
                    public Game create(dev.chunkzero.runtime.SessionCreation<fixture.generated.Bindings> creation) { return new Game(); }
                    public static void main(String[] args) {
                        Verify.run(new App().create(new dev.chunkzero.runtime.SessionCreation<>(32, new fixture.generated.Bindings("ok"))));
                    }
                }
                """,
            )
        }
    }

    private fun methodFixture(kotlin: Boolean) {
        fixture(kotlin)
        app("lobby", kotlin)
        write("method-fixture", "")
        write(
            "apps/lobby/src/main/java/dev/chunkzero/runtime/Session.java",
            """
            package dev.chunkzero.runtime;
            public abstract class Session {}
        """,
        )
        write(
            "apps/lobby/src/main/java/dev/chunkzero/runtime/SessionMethodBinding.java",
            """
            package dev.chunkzero.runtime;
            public final class SessionMethodBinding<A, R> {
                private final java.util.function.BiFunction<Session,A,R> body;
                public SessionMethodBinding(fixture.generated.Ping.Ref<A,R> ref, java.util.function.BiFunction<Session,A,R> body) { this.body=body; }
                public R invoke(Session session, A args) { return body.apply(session,args); }
            }
        """,
        )
        write(
            "apps/lobby/src/main/java/dev/chunkzero/runtime/SessionMethodProvider.java",
            """
            package dev.chunkzero.runtime;
            public interface SessionMethodProvider {
                java.util.Collection<SessionMethodBinding<?,?>> methods();
            }
        """,
        )
        write(
            "apps/lobby/src/main/java/Verify.java",
            """
            package fixture.lobby;
            public final class Verify {
                @SuppressWarnings("unchecked")
                public static void run(dev.chunkzero.runtime.Session session) {
                    var provider=java.util.ServiceLoader.load(dev.chunkzero.runtime.SessionMethodProvider.class).findFirst().orElseThrow();
                    var method=(dev.chunkzero.runtime.SessionMethodBinding<fixture.generated.Bindings,String>) provider.methods().iterator().next();
                    System.out.println(method.invoke(session,new fixture.generated.Bindings("ok")));
                }
            }
        """,
        )
        if (kotlin) {
            write(
                "apps/lobby/src/main/kotlin/App.kt",
                """
                package fixture.lobby
                @dev.chunkzero.runtime.SessionType("default")
                class Factory : dev.chunkzero.runtime.SessionProvider {
                    fun create() = Game()
                }
                class Game : dev.chunkzero.runtime.Session(), fixture.generated.Ping {
                    override fun ping(args: fixture.generated.Bindings): String = "echo:" + args.value()
                }
                fun main() { Verify.run(Factory().create()) }
            """,
            )
        } else {
            write(
                "apps/lobby/src/main/java/App.java",
                """
                package fixture.lobby;
                @dev.chunkzero.runtime.SessionType("default")
                public final class App implements dev.chunkzero.runtime.SessionProvider {
                    public Game create() { return new Game(); }
                    public static void main(String[] args) { Verify.run(new App().create()); }
                }
            """,
            )
            write(
                "apps/lobby/src/main/java/Game.java",
                """
                package fixture.lobby;
                public final class Game extends dev.chunkzero.runtime.Session implements fixture.generated.Ping {
                    public String ping(fixture.generated.Bindings args) { return "echo:" + args.value(); }
                }
            """,
            )
        }
    }

    private fun assertMethodExecution() {
        val jar = descriptor().getAsJsonArray("apps")[0].asJsonObject["jar"].asString
        JarFile(jar).use {
            val manifest = requireNotNull(it.getEntry("META-INF/chunk/session-methods.json"))
            val metadata = JsonParser.parseString(it.getInputStream(manifest).bufferedReader().readText()).asJsonObject
            assertEquals("ping", metadata.getAsJsonArray("methods")[0].asJsonObject["name"].asString)
        }
        val process =
            ProcessBuilder("${System.getProperty("chunk.test.java.home")}/bin/java", "-jar", jar)
                .redirectErrorStream(true)
                .start()
        val output = process.inputStream.bufferedReader().use { it.readText() }
        assertEquals(0, process.waitFor(), output)
        assertEquals("echo:ok", output.trim())
    }

    private fun fixture(kotlin: Boolean = false) {
        val kotlinVersion = System.getProperty("chunk.kotlin.version")
        val pluginVersion = System.getProperty("chunk.plugin.version")
        val pluginRepository = System.getProperty("chunk.test.plugin.repository")
        val kotlinPlugin = if (kotlin) "id(\"org.jetbrains.kotlin.jvm\") version \"$kotlinVersion\" apply false" else ""
        val plugin = if (kotlin) "dev.chunkzero.chunk.kotlin" else "dev.chunkzero.chunk"
        write(
            "settings.gradle.kts",
            """
            import dev.chunkzero.gradle.ChunkSettingsExtension
            pluginManagement { repositories {
                maven { url = uri("$pluginRepository") }
                gradlePluginPortal()
                mavenCentral()
            } }
            plugins {
                $kotlinPlugin
                id("dev.chunkzero.chunk.settings") version "$pluginVersion"
            }
            extensions.configure<ChunkSettingsExtension> {
                executable.set(file("chunk-fixture").absolutePath)
                javaPackage.set("fixture.generated")
            }
            dependencyResolutionManagement { repositories { maven { url = uri("maven") }; mavenCentral() } }
            rootProject.name = "fixture"
        """,
        )
        write(
            "build.gradle.kts",
            """
            plugins {
                id("$plugin")
            }
            java { toolchain.languageVersion = JavaLanguageVersion.of(25) }
        """,
        )
        write(
            "gradle.properties",
            """
            org.gradle.configuration-cache=true
            org.gradle.jvmargs=-Xmx512m
            org.gradle.workers.max=2
            org.gradle.java.installations.paths=${System.getProperty("chunk.test.java.home")}
            kotlin.compiler.execution.strategy=in-process
        """,
        )
        write("chunk.toml", "")
        listOf(
            "backend-client",
            "runtime-minestom",
            "backend-client-kotlin",
            "runtime-minestom-kotlin",
        ).forEach(::module)
        write(
            "chunk-fixture",
            """
            #!/usr/bin/env python3
            import json, pathlib, sys
            root = pathlib.Path(sys.argv[2])
            command = sys.argv[1]
            target = sys.argv[sys.argv.index('--target') + 1] if command == 'gen' else None
            with (root / 'calls.txt').open('a') as log:
                log.write(command + (' ' + target if target else '') + '\n')
            if command == 'inspect':
                apps = sorted(path.parent.name for path in root.glob('apps/*/app.toml'))
                print(json.dumps({'version': 1, 'apps': [
                    {'id': app, 'directory': 'apps/' + app, 'gradle_project': ':apps:' + app,
                     'sessions': {'default': {'machine_profile': 'large', 'capacity': 32}}
                     if (root / 'apps' / app / 'app.toml').read_text().strip() else {}}
                    for app in apps]}))
            elif command == 'gen':
                output = pathlib.Path(sys.argv[sys.argv.index('--output') + 1])
                backend = pathlib.Path(sys.argv[sys.argv.index('--backend-output') + 1])
                files = {
                    'java/fixture/generated/Bindings.java': 'package fixture.generated; public record Bindings(String value) {}',
                    'java-client/fixture/generated/Client.java': 'package fixture.generated; public final class Client { public static Bindings value() { return new Bindings("ok"); } }',
                }
                if target == 'kotlin':
                    files['kotlin/fixture/generated/Facade.kt'] = 'package fixture.generated\nsuspend fun value(): Bindings = Client.value()\n'
                methods = []
                configurations = []
                if (root / 'method-fixture').is_file():
                    files['java/fixture/generated/Ping.java'] = 'package fixture.generated; public interface Ping { String ping(Bindings args); record Ref<A,R>() {} Ref<Bindings,String> REF = new Ref<>(); }'
                    methods = [{'app':'lobby','session':'default','name':'ping','interface':'fixture.generated.Ping','binary_interface':'fixture.generated.Ping','function':'ping','arguments':{'type':'object','fields':{}},'result':{'type':'string'}}]
                if (root / 'configuration-fixture').is_file():
                    files['java-session/lobby/fixture/generated/LobbySessionProviders.java'] = 'package fixture.generated; public final class LobbySessionProviders { public interface Default extends dev.chunkzero.runtime.ConfiguredSessionProvider<Bindings> { default dev.chunkzero.backend.api.JsonType<Bindings> configurationType() { return null; } } }'
                    configurations = [{'app':'lobby','session':'default','interface':'fixture.generated.LobbySessionProviders.Default','binary_interface':'fixture.generated.LobbySessionProviders${'$'}Default','configuration':{'type':'object','fields':{'value':{'schema':{'type':'string'}}}}}]
                for name, content in files.items():
                    path = output / name
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_text(content)
                backend.mkdir(parents=True, exist_ok=True)
                (backend / 'contract.json').write_text('{}')
                (output / 'session-methods.json').write_text(json.dumps({'version': 1, 'methods': methods}))
                (output / 'session-configurations.json').write_text(json.dumps({'version': 1, 'configurations': configurations}))
            else:
                raise SystemExit('Unexpected command: ' + command)
        """,
        )
        check(directory.resolve("chunk-fixture").toFile().setExecutable(true))
    }

    private fun app(
        id: String,
        kotlin: Boolean = false,
        toolchain: String = "java { toolchain.languageVersion = JavaLanguageVersion.of(25) }",
    ) {
        write("apps/$id/app.toml", "")
        val plugin = if (kotlin) "dev.chunkzero.chunk.kotlin" else "dev.chunkzero.chunk"
        write(
            "apps/$id/build.gradle.kts",
            """
            plugins { id("$plugin") }
            $toolchain
            application { mainClass = "fixture.$id.App${if (kotlin) "Kt" else ""}" }
        """,
        )
        write(
            "apps/$id/src/main/java/dev/chunkzero/runtime/SessionType.java",
            """
            package dev.chunkzero.runtime;
            @java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.RUNTIME)
            @java.lang.annotation.Target(java.lang.annotation.ElementType.TYPE)
            public @interface SessionType {
                String value();
            }
        """,
        )
        write(
            "apps/$id/src/main/java/dev/chunkzero/runtime/SessionProvider.java",
            """
            package dev.chunkzero.runtime;
            public interface SessionProvider {}
        """,
        )
        if (kotlin) {
            write(
                "apps/$id/src/main/kotlin/App.kt",
                """
                package fixture.$id
                @dev.chunkzero.runtime.SessionType("default")
                class Factory : dev.chunkzero.runtime.SessionProvider
                fun main(args: Array<String>) {}
                suspend fun result(): fixture.generated.Bindings = fixture.generated.value()
            """,
            )
        } else {
            write(
                "apps/$id/src/main/java/App.java",
                """
                package fixture.$id;
                @dev.chunkzero.runtime.SessionType("default")
                public final class App implements dev.chunkzero.runtime.SessionProvider {
                    public static void main(String[] args) {}
                    public fixture.generated.Bindings value() { return fixture.generated.Client.value(); }
                }
            """,
            )
        }
    }

    private fun module(name: String) {
        val version = System.getProperty("chunk.plugin.version")
        library("dev.chunkzero", name, version)
    }

    private fun library(
        group: String,
        name: String,
        version: String,
    ) {
        val path = "maven/${group.replace('.', '/')}/$name/$version/$name-$version"
        write(
            "$path.pom",
            """
            <project><modelVersion>4.0.0</modelVersion><groupId>$group</groupId>
            <artifactId>$name</artifactId><version>$version</version></project>
        """,
        )
        JarOutputStream(directory.resolve("$path.jar").toFile().outputStream()).use { jar ->
            if (group == "fixture" && name == "shared") {
                jar.putNextEntry(java.util.jar.JarEntry("fixture/shared-version.txt"))
                jar.write(version.toByteArray())
                jar.closeEntry()
            }
        }
    }

    private fun write(
        path: String,
        content: String,
    ) {
        val file = directory.resolve(path).toFile()
        file.parentFile.mkdirs()
        file.writeText(content.trimIndent() + "\n")
    }

    private fun runner(vararg arguments: String) =
        GradleRunner
            .create()
            .withProjectDir(directory.toFile())
            .withArguments(*arguments, "--console=plain", "--stacktrace")

    private fun run(vararg arguments: String) = runner(*arguments).build()

    private fun runFailure(vararg arguments: String) = runner(*arguments).buildAndFail()

    private fun calls() = directory.resolve("calls.txt").toFile().readLines()

    private fun assertSessionRegistries(artifacts: JsonObject) {
        assertEquals(3, artifacts["version"].asInt)
        artifacts.getAsJsonArray("apps").forEach { app ->
            assertEquals(listOf("default"), app.asJsonObject.getAsJsonArray("sessions").map { it.asString })
            JarFile(app.asJsonObject["jar"].asString).use { jar ->
                assertTrue(jar.getJarEntry("META-INF/chunk/app.json") == null)
                val entry = requireNotNull(jar.getJarEntry("META-INF/services/dev.chunkzero.runtime.SessionProvider"))
                val providers = jar.getInputStream(entry).bufferedReader().use { it.readLines() }
                assertEquals(1, providers.size)
                assertTrue(jar.getJarEntry(providers.single().replace('.', '/') + ".class") != null)
                val main = requireNotNull(jar.manifest.mainAttributes.getValue("Main-Class"))
                assertTrue(jar.getJarEntry(main.replace('.', '/') + ".class") != null)
            }
        }
    }

    private fun descriptor(): JsonObject =
        JsonParser
            .parseString(
                directory.resolve(".chunk/build/jvm/artifacts.json").toFile().readText(),
            ).asJsonObject
}
