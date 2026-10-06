package com.chunkzero.chunk.gradle

import com.google.gson.Gson
import org.gradle.api.DefaultTask
import org.gradle.api.file.ConfigurableFileCollection
import org.gradle.api.file.DirectoryProperty
import org.gradle.api.file.RegularFileProperty
import org.gradle.api.provider.Property
import org.gradle.api.tasks.CacheableTask
import org.gradle.api.tasks.Classpath
import org.gradle.api.tasks.Input
import org.gradle.api.tasks.InputFiles
import org.gradle.api.tasks.OutputDirectory
import org.gradle.api.tasks.OutputFile
import org.gradle.api.tasks.PathSensitive
import org.gradle.api.tasks.PathSensitivity
import org.gradle.api.tasks.TaskAction

@CacheableTask
abstract class WriteSessionRegistry : DefaultTask() {
    @get:Input abstract val app: Property<String>

    @get:Input abstract val mainClass: Property<String>

    @get:InputFiles
    @get:PathSensitive(PathSensitivity.RELATIVE)
    abstract val classes: ConfigurableFileCollection

    @get:Classpath abstract val dependencies: ConfigurableFileCollection

    @get:OutputDirectory abstract val outputDirectory: DirectoryProperty

    @get:InputFiles
    @get:PathSensitive(PathSensitivity.NONE)
    abstract val methodContracts: ConfigurableFileCollection

    @get:InputFiles
    @get:PathSensitive(PathSensitivity.NONE)
    abstract val configurationContracts: ConfigurableFileCollection

    @get:OutputDirectory abstract val bindingSourceDirectory: DirectoryProperty

    @get:OutputFile abstract val catalogFile: RegularFileProperty

    @TaskAction
    fun write() {
        val found = sortedMapOf<String, CompiledClass>()
        classes.files.filter { it.isDirectory }.forEach { directory ->
            directory.walkTopDown().filter { it.isFile && it.extension == "class" }.forEach {
                val type = inspectClass(it.readBytes())
                found[type.name] = type
            }
        }
        val main = found[mainClass.get().replace('.', '/')]
        require(main?.main == true) { "App requires a compiled public static main(String[]): ${mainClass.get()}" }
        val lookup = classLookup(dependencies.files, found)
        val sessions = sortedMapOf<String, String>()
        for (type in found.values.filter { it.session != null }) {
            val id = requireNotNull(type.session)
            require(id.matches(Regex("[A-Za-z_][A-Za-z0-9_]{0,127}"))) { "Invalid session type ID: $id" }
            require(
                type.constructible && lookup.inherits(type.name, "com/chunkzero/chunk/runtime/SessionProvider"),
            ) {
                "Session $id requires a public concrete SessionProvider with a public no-argument constructor"
            }
            require(sessions.put(id, type.name.replace('/', '.')) == null) { "Duplicate session type: $id" }
        }
        require(sessions.size in 1..128) { "App requires 1–128 @SessionType declarations" }
        writeSessionConfigurations(
            app.get(),
            sessions,
            configurationContracts.singleFile,
            lookup,
            outputDirectory.get().asFile,
        )
        writeSessionMethods(
            app.get(),
            sessions,
            methodContracts.singleFile,
            lookup,
            outputDirectory.get().asFile,
            bindingSourceDirectory.get().asFile,
        )
        val output =
            outputDirectory
                .file(
                    "META-INF/services/com.chunkzero.chunk.runtime.SessionProvider",
                ).get()
                .asFile
        output.parentFile.mkdirs()
        output.writeText(sessions.values.joinToString("\n", postfix = "\n"))
        val catalog = catalogFile.get().asFile
        catalog.parentFile.mkdirs()
        catalog.writeText(Gson().toJson(sessions.keys) + "\n")
    }
}
