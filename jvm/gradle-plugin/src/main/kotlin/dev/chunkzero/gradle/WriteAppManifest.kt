package dev.chunkzero.gradle

import com.google.gson.Gson
import org.gradle.api.DefaultTask
import org.gradle.api.file.DirectoryProperty
import org.gradle.api.provider.Property
import org.gradle.api.tasks.CacheableTask
import org.gradle.api.tasks.Input
import org.gradle.api.tasks.OutputDirectory
import org.gradle.api.tasks.TaskAction

@CacheableTask
abstract class WriteAppManifest : DefaultTask() {
    @get:Input
    abstract val app: Property<String>

    @get:OutputDirectory
    abstract val outputDirectory: DirectoryProperty

    @TaskAction
    fun write() {
        val output = outputDirectory.file("META-INF/chunk/app.json").get().asFile
        output.parentFile.mkdirs()
        output.writeText(Gson().toJson(mapOf("version" to 1, "id" to app.get())) + "\n")
    }
}
