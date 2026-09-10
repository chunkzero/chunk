package dev.chunkzero.gradle

import org.gradle.api.Plugin
import org.gradle.api.initialization.Settings
import org.gradle.api.model.ObjectFactory
import javax.inject.Inject

abstract class ChunkSettingsExtension
    @Inject
    constructor(
        objects: ObjectFactory,
    ) {
        val projectDirectory = objects.directoryProperty()
        val executable = objects.property(String::class.java)
        val javaPackage = objects.property(String::class.java).convention("dev.chunkzero.generated")
    }

class ChunkSettingsPlugin : Plugin<Settings> {
    override fun apply(settings: Settings) {
        val extension = settings.extensions.create("chunk", ChunkSettingsExtension::class.java)
        extension.projectDirectory.set(settings.settingsDir)
        extension.executable.convention("chunk")
        settings.gradle.settingsEvaluated {
            val directory =
                extension.projectDirectory
                    .get()
                    .asFile.canonicalFile
            val executable =
                settings.providers
                    .gradleProperty("chunk.executable")
                    .orElse(extension.executable)
                    .get()
            val metadata =
                settings.providers
                    .of(ProjectInspection::class.java) {
                        parameters.projectDirectory.set(directory)
                        parameters.executable.set(executable)
                    }.get()
            val apps = readApps(metadata, directory)
            apps.forEach { app ->
                check(
                    settings.findProject(app.projectPath) == null,
                ) { "App project already included: ${app.projectPath}" }
                settings.include(app.projectPath)
                settings.project(app.projectPath).projectDir = directory.resolve(app.directory)
            }
            check(
                settings.findProject(KOTLIN_BINDINGS) == null,
            ) { "$KOTLIN_BINDINGS is reserved for generated bindings" }
            val bindingsDirectory = directory.resolve(".chunk/gradle/backend-kotlin")
            bindingsDirectory.mkdirs()
            check(bindingsDirectory.isDirectory) { "Cannot create $bindingsDirectory" }
            settings.include(KOTLIN_BINDINGS)
            settings.project(":chunk").projectDir = bindingsDirectory.parentFile
            settings.project(KOTLIN_BINDINGS).projectDir = bindingsDirectory
            val configuration = BuildConfiguration(directory, executable, extension.javaPackage.get(), apps)
            settings.gradle.beforeProject {
                extensions.add(BuildConfiguration::class.java, "chunkBuildConfiguration", configuration)
            }
        }
    }
}

internal const val KOTLIN_BINDINGS = ":chunk:backend-kotlin"
