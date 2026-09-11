package dev.chunkzero.gradle

import com.google.gson.Gson
import org.gradle.api.DefaultTask
import org.gradle.api.file.RegularFileProperty
import org.gradle.api.provider.Property
import org.gradle.api.tasks.Input
import org.gradle.api.tasks.InputFile
import org.gradle.api.tasks.OutputFile
import org.gradle.api.tasks.PathSensitive
import org.gradle.api.tasks.PathSensitivity
import org.gradle.api.tasks.TaskAction
import org.gradle.work.DisableCachingByDefault

@DisableCachingByDefault(because = "The descriptor records machine-local build inputs")
abstract class WriteChunkModule : DefaultTask() {
    @get:Input
    abstract val app: Property<String>

    @get:Input
    abstract val projectPath: Property<String>

    @get:Input
    abstract val javaVersion: Property<Int>

    @get:Input
    abstract val javaExecutable: Property<String>

    @get:InputFile
    @get:PathSensitive(PathSensitivity.ABSOLUTE)
    abstract val jarFile: RegularFileProperty

    @get:OutputFile
    abstract val outputFile: RegularFileProperty

    @TaskAction
    fun write() {
        val gson = Gson()
        val output = outputFile.get().asFile
        output.parentFile.mkdirs()
        output.writeText(
            gson.toJson(
                ModuleArtifact(
                    app.get(),
                    projectPath.get(),
                    jarFile.get().asFile.absolutePath,
                    javaVersion.get(),
                    javaExecutable.get(),
                ),
            ),
        )
    }
}
