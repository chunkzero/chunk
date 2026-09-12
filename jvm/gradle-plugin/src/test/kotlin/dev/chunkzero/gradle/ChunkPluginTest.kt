package dev.chunkzero.gradle

import com.google.gson.JsonObject
import com.google.gson.JsonParser
import org.gradle.testkit.runner.GradleRunner
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import org.junit.jupiter.api.io.TempDir
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
        assertAppManifests(first)
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
        assertAppManifests(descriptor())
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
        assertAppManifests(descriptor())
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
    fun `session annotations fix placement requirements and reject duplicate IDs`() {
        fixture()
        app("lobby")
        val source = directory.resolve("apps/lobby/src/main/java/App.java").toFile()
        source.writeText(
            source.readText().replace(
                "SessionType(\"default\")",
                "SessionType(value = \"default\", machineProfile = \"large\", capacity = 32)",
            ),
        )
        run("chunkArtifacts")
        JarFile(descriptor().getAsJsonArray("apps")[0].asJsonObject["jar"].asString).use { jar ->
            val manifest =
                jar.getInputStream(jar.getEntry("META-INF/chunk/app.json")).bufferedReader().use {
                    JsonParser.parseReader(it).asJsonObject
                }
            val session = manifest.getAsJsonObject("sessions").getAsJsonObject("default")
            assertEquals("large", session["machine_profile"].asString)
            assertEquals(32, session["capacity"].asInt)
        }
        write(
            "apps/lobby/src/main/java/Duplicate.java",
            """
            package fixture.lobby;
            @dev.chunkzero.runtime.SessionType("default")
            public final class Duplicate implements dev.chunkzero.runtime.SessionProvider {}
        """,
        )
        assertTrue(runFailure(":apps:lobby:generateChunkAppManifest").output.contains("Duplicate session type"))
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
                    {'id': app, 'directory': 'apps/' + app, 'gradle_project': ':apps:' + app, 'runtime': {}}
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
                for name, content in files.items():
                    path = output / name
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_text(content)
                backend.mkdir(parents=True, exist_ok=True)
                (backend / 'contract.json').write_text('{}')
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
            @java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.CLASS)
            @java.lang.annotation.Target(java.lang.annotation.ElementType.TYPE)
            public @interface SessionType {
                String value(); String machineProfile() default ""; int capacity() default 0;
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

    private fun assertAppManifests(artifacts: JsonObject) {
        artifacts.getAsJsonArray("apps").forEach { app ->
            JarFile(app.asJsonObject["jar"].asString).use { jar ->
                val entry = requireNotNull(jar.getJarEntry("META-INF/chunk/app.json"))
                val manifest =
                    jar
                        .getInputStream(
                            entry,
                        ).bufferedReader()
                        .use { JsonParser.parseReader(it).asJsonObject }
                assertEquals(setOf("version", "id", "main_class", "sessions"), manifest.keySet())
                assertEquals(2, manifest["version"].asInt)
                assertEquals(manifest["main_class"].asString, jar.manifest.mainAttributes.getValue("Main-Class"))
                assertEquals(setOf("default"), manifest.getAsJsonObject("sessions").keySet())
                assertEquals(app.asJsonObject["id"].asString, manifest["id"].asString)
            }
        }
    }

    private fun descriptor(): JsonObject =
        JsonParser
            .parseString(
                directory.resolve(".chunk/build/jvm/artifacts.json").toFile().readText(),
            ).asJsonObject
}
