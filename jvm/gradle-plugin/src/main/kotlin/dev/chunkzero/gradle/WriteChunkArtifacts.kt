package dev.chunkzero.gradle

import com.google.gson.GsonBuilder
import org.gradle.api.DefaultTask
import org.gradle.api.file.ConfigurableFileCollection
import org.gradle.api.file.RegularFileProperty
import org.gradle.api.provider.ListProperty
import org.gradle.api.tasks.Input
import org.gradle.api.tasks.InputFiles
import org.gradle.api.tasks.OutputFile
import org.gradle.api.tasks.PathSensitive
import org.gradle.api.tasks.PathSensitivity
import org.gradle.api.tasks.TaskAction
import org.gradle.work.DisableCachingByDefault
import java.io.File

@DisableCachingByDefault(because = "The descriptor records machine-local build inputs")
abstract class WriteChunkArtifacts : DefaultTask() {
    @get:Input
    abstract val appIds: ListProperty<String>

    @get:InputFiles
    @get:PathSensitive(PathSensitivity.NONE)
    abstract val modules: ConfigurableFileCollection

    @get:OutputFile
    abstract val outputFile: RegularFileProperty

    @TaskAction
    fun write() {
        val gson = GsonBuilder().setPrettyPrinting().create()
        val artifacts =
            modules.files
                .map {
                    gson.fromJson(it.readText(), ModuleArtifact::class.java)
                }.sortedBy { it.projectPath }
        val apps = artifacts.filter { it.app.isNotEmpty() }.sortedBy { it.app }
        val found = apps.map { it.app }
        check(found == appIds.get().sorted()) {
            "Every discovered app must apply a Chunk project plugin exactly once: " +
                "expected ${appIds.get()}, found $found"
        }
        val appJars = apps.map { File(it.jar).canonicalFile }.toSet()
        val classpath =
            artifacts
                .flatMap { module ->
                    module.classpath +
                        ClasspathEntry(
                            module.jar,
                            File(module.jar).name,
                            mapOf(
                                "kind" to "project",
                                "build" to ":",
                                "path" to module.projectPath,
                            ),
                        )
                }.filter { File(it.file).canonicalFile !in appJars }
                .distinctBy { File(it.file).canonicalFile }
                .sortedWith(compareBy({ it.component.toString() }, { it.artifact }))
        val java = requireNotNull(artifacts.maxByOrNull { it.javaVersion }) { "No compiled JVM modules" }
        val output = outputFile.get().asFile
        output.parentFile.mkdirs()
        output.writeText(
            gson.toJson(
                mapOf(
                    "version" to 1,
                    "java" to mapOf("version" to java.javaVersion, "executable" to java.javaExecutable),
                    "apps" to apps.map { mapOf("id" to it.app, "jar" to it.jar, "java_version" to it.javaVersion) },
                    "classpath" to classpath,
                ),
            ) + "\n",
        )
    }
}
