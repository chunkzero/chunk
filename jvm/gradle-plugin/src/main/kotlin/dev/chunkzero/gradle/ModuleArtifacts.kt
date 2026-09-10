package dev.chunkzero.gradle

import com.google.gson.Gson
import org.gradle.api.Project
import org.gradle.api.artifacts.component.ModuleComponentIdentifier
import org.gradle.api.artifacts.component.ProjectComponentIdentifier
import org.gradle.api.plugins.JavaPluginExtension
import org.gradle.jvm.tasks.Jar
import org.gradle.jvm.toolchain.JavaToolchainService

internal fun configureModule(
    project: Project,
    appId: String,
) {
    val java = project.extensions.getByType(JavaPluginExtension::class.java)
    val launcher = project.extensions.getByType(JavaToolchainService::class.java).launcherFor(java.toolchain)
    val runtime = project.configurations.named("runtimeClasspath")
    val artifacts =
        runtime
            .get()
            .incoming.artifacts.resolvedArtifacts
    val descriptor =
        project.tasks.register("chunkModule", WriteChunkModule::class.java) {
            app.set(appId)
            projectPath.set(project.path)
            javaVersion.set(java.toolchain.languageVersion.map { it.asInt() })
            javaExecutable.set(launcher.map { it.executablePath.asFile.absolutePath })
            jarFile.set(project.tasks.named("jar", Jar::class.java).flatMap { it.archiveFile })
            classpath.from(runtime)
            classpathJson.set(
                artifacts.map { resolved ->
                    Gson().toJson(
                        resolved
                            .map { artifact ->
                                val component =
                                    when (val id = artifact.id.componentIdentifier) {
                                        is ModuleComponentIdentifier -> {
                                            mapOf(
                                                "kind" to "module",
                                                "group" to id.group,
                                                "name" to id.module,
                                                "version" to id.version,
                                            )
                                        }

                                        is ProjectComponentIdentifier -> {
                                            mapOf(
                                                "kind" to "project",
                                                "build" to id.build.buildPath,
                                                "path" to id.projectPath,
                                            )
                                        }

                                        else -> {
                                            error("Unsupported JVM dependency component: $id")
                                        }
                                    }
                                ClasspathEntry(artifact.file.absolutePath, artifact.file.name, component)
                            }.sortedWith(compareBy({ it.component.toString() }, { it.artifact })),
                    )
                },
            )
            outputFile.set(project.layout.buildDirectory.file("chunk/module.json"))
            dependsOn(project.tasks.named("validateChunkJvm"))
        }
    project.rootProject.tasks.named("chunkArtifacts", WriteChunkArtifacts::class.java) {
        modules.from(descriptor.flatMap { it.outputFile })
    }
}

internal data class ClasspathEntry(
    val file: String,
    val artifact: String,
    val component: Map<String, String>,
)

internal data class ModuleArtifact(
    val app: String,
    val projectPath: String,
    val jar: String,
    val javaVersion: Int,
    val javaExecutable: String,
    val classpath: List<ClasspathEntry>,
)
