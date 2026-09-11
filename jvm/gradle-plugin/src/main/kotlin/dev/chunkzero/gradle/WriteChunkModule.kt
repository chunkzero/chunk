package dev.chunkzero.gradle

import com.google.gson.Gson
import com.google.gson.reflect.TypeToken
import org.gradle.api.DefaultTask
import org.gradle.api.file.ConfigurableFileCollection
import org.gradle.api.file.RegularFileProperty
import org.gradle.api.provider.Property
import org.gradle.api.tasks.Classpath
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

    @get:Classpath
    abstract val classpath: ConfigurableFileCollection

    @get:Input
    abstract val classpathJson: Property<String>

    @get:OutputFile
    abstract val outputFile: RegularFileProperty

    @TaskAction
    fun write() {
        val gson = Gson()
        val dependencies: List<ClasspathEntry> =
            gson.fromJson(
                classpathJson.get(),
                object : TypeToken<List<ClasspathEntry>>() {}.type,
            )
        require(dependencies.all { it.file.endsWith(".jar") }) { "JVM runtime dependencies must be JARs" }
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
                    dependencies,
                ),
            ),
        )
    }
}
