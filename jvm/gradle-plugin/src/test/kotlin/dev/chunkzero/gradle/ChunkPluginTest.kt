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
    fun `discovers apps without generation and builds one shared Java artifact with configuration cache`() {
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
        val classpath = first.getAsJsonArray("classpath").map { it.asJsonObject }
        assertEquals(1, classpath.count { it["artifact"].asString == "chunk-backend.jar" })
        assertEquals(
            listOf("shared-2.0.jar"),
            classpath.map { it["artifact"].asString }.filter { it.startsWith("shared-") },
        )
        assertTrue(classpath.all { Path.of(it["file"].asString).toFile().isFile })
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
        assertEquals(
            listOf("shared-1.0.jar", "shared-2.0.jar"),
            descriptor()
                .getAsJsonArray("classpath")
                .map { it.asJsonObject["artifact"].asString }
                .filter { it.startsWith("shared-") }
                .sorted(),
        )
    }

    @Test
    fun `Kotlin opt in builds a facade separately from shared Java classes`() {
        fixture(kotlin = true)
        app("lobby", kotlin = true)
        run("chunkArtifacts")
        assertAppManifests(descriptor())
        val classpath = descriptor().getAsJsonArray("classpath").map { it.asJsonObject }
        val facade = classpath.single { it["artifact"].asString == "chunk-backend-kotlin.jar" }
        JarFile(facade["file"].asString).use {
            assertTrue(it.getEntry("fixture/generated/FacadeKt.class") != null)
            assertTrue(it.getEntry("fixture/generated/Bindings.class") == null)
            assertTrue(it.getEntry("META-INF/chunk/app.json") == null)
        }
        assertEquals(1, classpath.count { it["artifact"].asString == "chunk-backend.jar" })
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
        listOf("backend-client", "runtime", "backend-client-kotlin", "runtime-kotlin").forEach(::module)
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
        """,
        )
        if (kotlin) {
            write(
                "apps/$id/src/main/kotlin/App.kt",
                """
                package fixture.$id
                suspend fun result(): fixture.generated.Bindings = fixture.generated.value()
            """,
            )
        } else {
            write(
                "apps/$id/src/main/java/App.java",
                """
                package fixture.$id;
                public final class App {
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
        JarOutputStream(directory.resolve("$path.jar").toFile().outputStream()).close()
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
                assertEquals(setOf("version", "id"), manifest.keySet())
                assertEquals(1, manifest["version"].asInt)
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
