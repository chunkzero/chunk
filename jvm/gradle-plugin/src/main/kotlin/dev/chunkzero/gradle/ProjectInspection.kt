package dev.chunkzero.gradle

import com.google.gson.JsonParser
import org.gradle.api.file.DirectoryProperty
import org.gradle.api.provider.Property
import org.gradle.api.provider.ValueSource
import org.gradle.api.provider.ValueSourceParameters
import org.gradle.process.ExecOperations
import java.io.ByteArrayOutputStream
import java.io.File
import javax.inject.Inject

abstract class ProjectInspection : ValueSource<String, ProjectInspection.Parameters> {
    interface Parameters : ValueSourceParameters {
        val projectDirectory: DirectoryProperty
        val executable: Property<String>
    }

    @get:Inject
    abstract val exec: ExecOperations

    override fun obtain(): String {
        val output = ByteArrayOutputStream()
        val errors = ByteArrayOutputStream()
        val result =
            exec.exec {
                commandLine(parameters.executable.get(), "inspect", parameters.projectDirectory.get().asFile)
                standardOutput = output
                errorOutput = errors
                isIgnoreExitValue = true
            }
        check(result.exitValue == 0) { "chunk inspect failed: ${errors.toString(Charsets.UTF_8)}" }
        return output.toString(Charsets.UTF_8)
    }
}

internal data class AppMetadata(
    val id: String,
    val directory: String,
    val projectPath: String,
    val anvilWorlds: List<AnvilWorld> = emptyList(),
)

/** An app's Anvil world, converted to Polar; [chunks] is `[fromX, fromZ, toX, toZ]` or empty. */
internal data class AnvilWorld(
    val name: String,
    val source: String,
    val chunks: List<Int>,
)

internal data class BuildConfiguration(
    val directory: File,
    val executable: String,
    val javaPackage: String,
    val apps: List<AppMetadata>,
)

internal fun readApps(
    metadata: String,
    directory: File,
): List<AppMetadata> {
    val root = JsonParser.parseString(metadata).asJsonObject
    require(root["version"]?.asInt == 1) { "Unsupported chunk inspect metadata version" }
    val apps =
        requireNotNull(root["apps"]?.asJsonArray) { "chunk inspect returned no apps inventory" }.map { value ->
            val app = value.asJsonObject
            val id = requireNotNull(app["id"]?.asString) { "App ID missing from chunk inspect" }
            require(id.matches(Regex("[A-Za-z_][A-Za-z0-9_]{0,127}"))) { "Invalid app ID: $id" }
            val path = requireNotNull(app["directory"]?.asString) { "App directory missing: $id" }
            val projectPath = requireNotNull(app["gradle_project"]?.asString) { "Gradle project missing: $id" }
            val segments = path.split('/')
            require(
                segments.size >= 2 &&
                    segments.first() == "apps" &&
                    segments.drop(1).all { it.matches(Regex("[A-Za-z_][A-Za-z0-9_]{0,127}")) } &&
                    projectPath == segments.joinToString(":", prefix = ":"),
            ) { "App inventory mapping mismatch: $id" }
            require(directory.resolve(path).isDirectory) { "App directory missing: $path" }
            AppMetadata(
                id,
                path,
                projectPath,
                app["worlds"]?.asJsonObject?.entrySet().orEmpty().mapNotNull { (name, value) ->
                    val world = value.asJsonObject
                    if (world["format"]?.asString != "anvil") return@mapNotNull null
                    require(name.matches(Regex("[A-Za-z_][A-Za-z0-9_]{0,127}"))) { "Invalid world name: $name" }
                    val source = requireNotNull(world["source"]?.asString) { "World source missing: $id/$name" }
                    val chunks =
                        world["chunks"]?.asJsonObject?.let { range ->
                            listOf("from", "to").flatMap { end -> range.getAsJsonArray(end).map { it.asInt } }
                        }
                    AnvilWorld(name, source, chunks.orEmpty())
                },
            )
        }
    require(apps.map { it.id.lowercase() }.distinct().size == apps.size) { "Duplicate app IDs in chunk inspect" }
    require(apps.map { it.projectPath.lowercase() }.distinct().size == apps.size) {
        "Duplicate app directories in chunk inspect"
    }
    return apps.sortedBy { it.id }
}
