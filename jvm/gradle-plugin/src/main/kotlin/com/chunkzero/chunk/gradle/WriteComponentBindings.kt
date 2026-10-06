package com.chunkzero.chunk.gradle

import com.google.gson.JsonParser
import org.gradle.api.DefaultTask
import org.gradle.api.file.ConfigurableFileCollection
import org.gradle.api.file.DirectoryProperty
import org.gradle.api.provider.Property
import org.gradle.api.tasks.CacheableTask
import org.gradle.api.tasks.Classpath
import org.gradle.api.tasks.Input
import org.gradle.api.tasks.InputFiles
import org.gradle.api.tasks.OutputDirectory
import org.gradle.api.tasks.PathSensitive
import org.gradle.api.tasks.PathSensitivity
import org.gradle.api.tasks.TaskAction
import java.io.File
import java.util.jar.JarFile

@CacheableTask
abstract class WriteComponentBindings : DefaultTask() {
    @get:Input abstract val app: Property<String>

    @get:InputFiles
    @get:PathSensitive(PathSensitivity.RELATIVE)
    abstract val classes: ConfigurableFileCollection

    @get:InputFiles
    @get:PathSensitive(PathSensitivity.RELATIVE)
    abstract val index: ConfigurableFileCollection

    @get:Classpath abstract val dependencies: ConfigurableFileCollection

    @get:OutputDirectory abstract val sourceDirectory: DirectoryProperty

    @get:OutputDirectory abstract val resourceDirectory: DirectoryProperty

    @TaskAction
    fun write() {
        val locations = classes.files + dependencies.files
        val names = sortedSetOf<String>()
        for (file in index.files + dependencies.files) {
            componentIndexes(file).forEach { contents ->
                val parsed = JsonParser.parseString(contents).asJsonObject
                require(parsed["version"]?.asInt == 1) { "Unsupported component index version" }
                val entries = requireNotNull(parsed["classes"]?.asJsonArray) { "Missing indexed component classes" }
                require(entries.size() <= 256) { "Too many indexed component classes" }
                for (entry in entries) {
                    val name = entry.asString
                    require(
                        name.length <= 1024 &&
                            name.matches(Regex("[A-Za-z_$][A-Za-z0-9_$]*(/[A-Za-z_$][A-Za-z0-9_$]*)*")),
                    ) {
                        "Invalid indexed component class"
                    }
                    require(names.add(name)) { "Duplicate indexed component class: $name" }
                }
                require(names.size <= 256) { "Too many indexed component classes" }
            }
        }
        val methods =
            names.flatMap { name ->
                val bytes =
                    requireNotNull(locations.firstNotNullOfOrNull { readClass(it, name) }) {
                        "Indexed component class is missing: $name"
                    }
                inspectComponents(bytes).also {
                    require(it.isNotEmpty() && it.all { method -> method.owner == name }) {
                        "Indexed component class has no matching declarations: $name"
                    }
                }
            }
        val factories = validateComponents(componentFactories(methods))
        val sources = sourceDirectory.get().asFile
        val resources = resourceDirectory.get().asFile
        sources.deleteRecursively()
        resources.deleteRecursively()
        sources.mkdirs()
        resources.mkdirs()
        if (factories.isEmpty()) return
        val namespace = "com.chunkzero.chunk.generated.components.a${app.get().toByteArray().joinToString(
            "",
        ) { "%02x".format(it) }}"
        val bindings =
            factories.map { factory ->
                val dependencies = factory.dependencies.joinToString(", ") { "${sourceType(it)}.class" }
                val arguments = factory.dependencies.mapIndexed { index, type -> "(${sourceType(type)}) args[$index]" }
                "new com.chunkzero.chunk.multistom.ComponentBinding<>(${sourceType(factory.type)}.class, " +
                    "com.chunkzero.chunk.runtime.Component.Scope.${factory.scope}, java.util.List.of($dependencies), " +
                    "args -> ${sourceType(factory.owner)}.${factory.name}(${arguments.joinToString(", ")}))"
            }
        val source = sources.resolve("${namespace.replace('.', '/')}/ChunkComponents.java")
        source.parentFile.mkdirs()
        source.writeText(
            """
            package $namespace;
            public final class ChunkComponents implements com.chunkzero.chunk.multistom.ComponentProvider {
                public java.util.Collection<com.chunkzero.chunk.multistom.ComponentBinding<?>> components() {
                    return java.util.List.of(${bindings.joinToString(",\n")});
                }
            }
            """.trimIndent() + "\n",
        )
        val service = resources.resolve("META-INF/services/com.chunkzero.chunk.multistom.ComponentProvider")
        service.parentFile.mkdirs()
        service.writeText("$namespace.ChunkComponents\n")
    }
}

private fun sourceType(binary: String) = binary.replace('/', '.').replace('$', '.')

private fun componentIndexes(file: File): List<String> {
    val prefix = "META-INF/chunk/components/"
    return if (file.isDirectory) {
        file
            .resolve(prefix)
            .walkTopDown()
            .filter { it.isFile && it.extension == "json" }
            .map {
                require(it.length() <= 256 * 1024) { "Component index exceeds limit" }
                it.readText()
            }.toList()
    } else if (file.isFile) {
        JarFile(file).use { jar ->
            jar
                .entries()
                .asSequence()
                .filter { !it.isDirectory && it.name.startsWith(prefix) && it.name.endsWith(".json") }
                .map {
                    jar.getInputStream(it).use { input ->
                        val bytes = input.readNBytes(256 * 1024 + 1)
                        require(bytes.size <= 256 * 1024) { "Component index exceeds limit" }
                        bytes.toString(Charsets.UTF_8)
                    }
                }.toList()
        }
    } else {
        emptyList()
    }
}

private fun readClass(
    file: File,
    name: String,
): ByteArray? =
    if (file.isDirectory) {
        file.resolve("$name.class").takeIf { it.isFile }?.readBytes()
    } else if (file.isFile) {
        JarFile(file).use { jar ->
            jar.getJarEntry("$name.class")?.let { jar.getInputStream(it).use { input -> input.readAllBytes() } }
        }
    } else {
        null
    }
