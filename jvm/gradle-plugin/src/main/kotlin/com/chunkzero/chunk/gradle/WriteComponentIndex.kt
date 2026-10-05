package com.chunkzero.chunk.gradle

import com.google.gson.Gson
import org.gradle.api.DefaultTask
import org.gradle.api.file.ConfigurableFileCollection
import org.gradle.api.file.DirectoryProperty
import org.gradle.api.tasks.CacheableTask
import org.gradle.api.tasks.InputFiles
import org.gradle.api.tasks.OutputDirectory
import org.gradle.api.tasks.PathSensitive
import org.gradle.api.tasks.PathSensitivity
import org.gradle.api.tasks.TaskAction

@CacheableTask
abstract class WriteComponentIndex : DefaultTask() {
    @get:InputFiles
    @get:PathSensitive(PathSensitivity.RELATIVE)
    abstract val classes: ConfigurableFileCollection

    @get:OutputDirectory abstract val outputDirectory: DirectoryProperty

    @TaskAction
    fun write() {
        val declarations =
            classes.files
                .filter { it.isDirectory }
                .flatMap { directory ->
                    directory
                        .walkTopDown()
                        .filter { it.isFile && it.extension == "class" }
                        .flatMap { inspectComponents(it.readBytes()) }
                        .map { it.owner }
                        .toList()
                }.distinct()
                .sorted()
        require(declarations.size <= 256) { "Module supports at most 256 component factory classes" }
        val output = outputDirectory.get().asFile
        output.deleteRecursively()
        output.mkdirs()
        // Module JARs are merged into the app shadow JAR, which fails on duplicate entries, so each
        // class gets its own index file rather than one shared name every module would collide on.
        for (declaration in declarations) {
            val index = output.resolve("META-INF/chunk/components/$declaration.json")
            index.parentFile.mkdirs()
            index.writeText(Gson().toJson(mapOf("version" to 1, "classes" to listOf(declaration))) + "\n")
        }
    }
}
