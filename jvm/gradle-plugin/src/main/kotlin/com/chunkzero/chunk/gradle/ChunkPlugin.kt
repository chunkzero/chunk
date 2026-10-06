package com.chunkzero.chunk.gradle

import org.gradle.api.Plugin
import org.gradle.api.Project
import org.gradle.api.Task
import org.gradle.api.file.SourceDirectorySet
import org.gradle.api.plugins.JavaLibraryPlugin
import org.gradle.api.plugins.JavaPluginExtension
import org.gradle.api.provider.Provider
import org.gradle.api.tasks.SourceSetContainer
import org.gradle.api.tasks.TaskProvider
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
                "Apply com.chunkzero.chunk to the root project before its consumers"
            }
            project.dependencies.add("implementation", project.dependencies.project(mapOf("path" to ":")))
            if (project.rootProject.pluginManager.hasPlugin(KOTLIN_PLUGIN)) {
                project.pluginManager.withPlugin(KOTLIN_PLUGIN) {
                    project.dependencies.add(
                        "implementation",
                        project.dependencies.project(mapOf("path" to KOTLIN_BINDINGS)),
                    )
                }
            }
        }
        val app = configuration.apps.find { it.projectPath == project.path }
        configureModule(project, app?.id.orEmpty())
        if (app != null) configureWorlds(project, configuration.directory, app)
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
        description = "Describe the executable app JARs and their Java requirements"
        appIds.set(configuration.apps.map { it.id })
        outputFile.set(configuration.directory.resolve(".chunk/build/jvm/artifacts.json"))
    }
    project.pluginManager.withPlugin(KOTLIN_PLUGIN) {
        // The root's own sources are Java, so Java apps need no Kotlin standard library through it.
        listOf("apiElements", "runtimeElements").forEach {
            project.configurations.named(it) {
                exclude(mapOf("group" to "org.jetbrains.kotlin", "module" to "kotlin-stdlib"))
            }
        }
        configureKotlinBindings(project, generate)
    }
}

// A Kotlin root also builds the coroutine facade in a separate project, so Java apps stay free of Kotlin.
private fun configureKotlinBindings(
    root: Project,
    generate: TaskProvider<GenerateChunkBackend>,
) {
    generate.configure { target.set("kotlin") }
    val project = root.project(KOTLIN_BINDINGS)
    project.pluginManager.apply(JavaLibraryPlugin::class.java)
    val rootJava = root.extensions.getByType(JavaPluginExtension::class.java)
    project.extensions
        .getByType(JavaPluginExtension::class.java)
        .toolchain.languageVersion
        .set(rootJava.toolchain.languageVersion)
    configureJava(project)
    project.pluginManager.apply(KOTLIN_PLUGIN)
    project.extensions.getByType(SourceSetContainer::class.java).named("main") {
        (extensions.getByName("kotlin") as SourceDirectorySet).srcDir(
            generate.flatMap { it.generatedDirectory.dir("kotlin") },
        )
    }
    project.tasks.named("compileKotlin") { dependsOn(generate) }
    project.tasks.named("jar", Jar::class.java) { archiveBaseName.set("chunk-backend-kotlin") }
    project.dependencies.add("api", project.dependencies.project(mapOf("path" to ":")))
    project.dependencies.add("api", framework("backend-client-kotlin"))
    configureModule(project, "")
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
    project.pluginManager.withPlugin(KOTLIN_PLUGIN) {
        // Kotlin may be applied from a build classloader this plugin cannot see, so its types come from the plugin.
        val loader =
            project.plugins
                .getPlugin(KOTLIN_PLUGIN)
                .javaClass.classLoader
        val compileType =
            Class
                .forName("org.jetbrains.kotlin.gradle.tasks.KotlinJvmCompile", false, loader)
                .asSubclass(Task::class.java)
        val compilerOptions = compileType.getMethod("getCompilerOptions")
        val jvmTarget =
            Class
                .forName("org.jetbrains.kotlin.gradle.dsl.KotlinJvmCompilerOptions", false, loader)
                .getMethod("getJvmTarget")
        validation.configure {
            project.tasks.withType(compileType).forEach { compile ->
                val target = jvmTarget.invoke(compilerOptions.invoke(compile)) as Provider<*>
                // JvmTarget constants are named JVM_1_8, JVM_25 and so on.
                targets.put(
                    compile.name,
                    target.map {
                        (it as Enum<*>)
                            .name
                            .removePrefix("JVM_")
                            .removePrefix("1_")
                            .toInt()
                    },
                )
            }
        }
        project.tasks.withType(compileType).configureEach { dependsOn(validation) }
    }
}

internal fun Project.configuration(): BuildConfiguration =
    requireNotNull(extensions.findByType(BuildConfiguration::class.java)) {
        "Apply com.chunkzero.chunk.settings in settings.gradle.kts before the Chunk project plugin"
    }

internal const val KOTLIN_PLUGIN = "org.jetbrains.kotlin.jvm"

internal fun framework(module: String): String {
    val version =
        checkNotNull(ChunkPlugin::class.java.`package`.implementationVersion) {
            "The Chunk Gradle plugin JAR has no Implementation-Version"
        }
    return "com.chunkzero.chunk:$module:$version"
}
