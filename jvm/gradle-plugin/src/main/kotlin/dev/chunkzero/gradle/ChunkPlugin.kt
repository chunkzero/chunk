package dev.chunkzero.gradle

import org.gradle.api.Plugin
import org.gradle.api.Project
import org.gradle.api.plugins.JavaLibraryPlugin
import org.gradle.api.plugins.JavaPluginExtension
import org.gradle.api.tasks.SourceSetContainer
import org.gradle.api.tasks.compile.JavaCompile
import org.gradle.jvm.tasks.Jar

class ChunkPlugin : Plugin<Project> {
    override fun apply(project: Project) {
        val configuration = project.configuration()
        project.pluginManager.apply(JavaLibraryPlugin::class.java)
        configureJava(project)
        if (project == project.rootProject) {
            configureRoot(project, configuration)
        } else {
            check(project.rootProject.plugins.hasPlugin(ChunkPlugin::class.java)) {
                "Apply dev.chunkzero.chunk to the root project before its consumers"
            }
            project.dependencies.add("implementation", project.dependencies.project(mapOf("path" to ":")))
            project.dependencies.add("implementation", framework("runtime"))
        }
        configureModule(
            project,
            configuration.apps
                .find { it.projectPath == project.path }
                ?.id
                .orEmpty(),
        )
    }
}

private fun configureRoot(
    project: Project,
    configuration: BuildConfiguration,
) {
    val generate =
        project.tasks.register("generateChunkBackend", GenerateChunkBackend::class.java) {
            group = "chunk"
            description = "Compile the backend and generate the selected shared JVM clients"
            projectDirectory.set(configuration.directory)
            executable.set(configuration.executable)
            javaPackage.set(configuration.javaPackage)
            target.convention("java")
            backendDirectory.set(configuration.directory.resolve(".chunk/build/backend"))
            generatedDirectory.set(configuration.directory.resolve(".chunk/generated/jvm"))
            outputs.upToDateWhen { false }
        }
    project.extensions.getByType(SourceSetContainer::class.java).named("main") {
        java.srcDir(generate.flatMap { it.generatedDirectory.dir("java") })
        java.srcDir(generate.flatMap { it.generatedDirectory.dir("java-client") })
    }
    project.tasks.named("compileJava") { dependsOn(generate) }
    project.tasks.named("jar", Jar::class.java) { archiveBaseName.set("chunk-backend") }
    project.dependencies.add("api", framework("backend-client"))
    project.tasks.register("chunkArtifacts", WriteChunkArtifacts::class.java) {
        group = "chunk"
        description = "Describe the compiled app JARs and complete JVM runtime classpath"
        appIds.set(configuration.apps.map { it.id })
        outputFile.set(configuration.directory.resolve(".chunk/build/jvm/artifacts.json"))
    }
}

internal fun configureJava(project: Project) {
    val java = project.extensions.getByType(JavaPluginExtension::class.java)
    val version = java.toolchain.languageVersion.map { it.asInt() }
    val validation =
        project.tasks.register("validateChunkJvm", ValidateChunkJvm::class.java) {
            projectName.set(project.path)
            javaVersion.set(version)
            targets.convention(emptyMap())
            project.tasks.withType(JavaCompile::class.java).forEach { compile ->
                targets.put(compile.name, compile.options.release.orElse(-1))
            }
        }
    project.tasks.withType(JavaCompile::class.java).configureEach {
        options.release.convention(version)
        dependsOn(validation)
    }
    project.tasks.withType(Jar::class.java).configureEach {
        isPreserveFileTimestamps = false
        isReproducibleFileOrder = true
    }
}

internal fun Project.configuration(): BuildConfiguration =
    requireNotNull(extensions.findByType(BuildConfiguration::class.java)) {
        "Apply dev.chunkzero.chunk.settings in settings.gradle.kts before the Chunk project plugin"
    }

internal fun framework(module: String): String {
    val version = ChunkPlugin::class.java.`package`.implementationVersion ?: "0.1.0"
    return "dev.chunkzero:$module:$version"
}
