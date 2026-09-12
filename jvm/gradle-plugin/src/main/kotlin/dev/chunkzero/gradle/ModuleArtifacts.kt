package dev.chunkzero.gradle

import com.github.jengelman.gradle.plugins.shadow.tasks.ShadowJar
import org.gradle.api.Project
import org.gradle.api.file.DuplicatesStrategy
import org.gradle.api.plugins.JavaApplication
import org.gradle.api.plugins.JavaPluginExtension
import org.gradle.api.tasks.SourceSetContainer
import org.gradle.jvm.tasks.Jar
import org.gradle.jvm.toolchain.JavaToolchainService

internal fun configureModule(
    project: Project,
    appId: String,
) {
    val registry =
        if (appId.isNotEmpty()) {
            project.pluginManager.apply("application")
            project.pluginManager.apply("com.gradleup.shadow")
            val application = project.extensions.getByType(JavaApplication::class.java)
            val sources = project.extensions.getByType(SourceSetContainer::class.java).named("main")
            val registry =
                project.tasks.register(
                    "generateChunkSessionRegistry",
                    WriteSessionRegistry::class.java,
                ) {
                    mainClass.set(application.mainClass)
                    classes.from(sources.map { it.output.classesDirs })
                    dependencies.from(project.configurations.named("compileClasspath"))
                    dependsOn(project.tasks.named("compileJava"))
                    project.plugins.withId(
                        "org.jetbrains.kotlin.jvm",
                    ) { dependsOn(project.tasks.named("compileKotlin")) }
                    outputDirectory.set(project.layout.buildDirectory.dir("generated/chunk/session-resources"))
                    catalogFile.set(project.layout.buildDirectory.file("chunk/sessions.json"))
                }
            sources.configure { resources.srcDir(registry.flatMap { it.outputDirectory }) }
            project.tasks.named("shadowJar", ShadowJar::class.java) {
                mergeServiceFiles()
                filesMatching(listOf("META-INF/services/**", "META-INF/*.kotlin_module")) {
                    duplicatesStrategy =
                        DuplicatesStrategy.INCLUDE
                }
                filesMatching("**/*.class") {
                    duplicatesStrategy =
                        DuplicatesStrategy.FAIL
                }
                exclude(
                    "module-info.class",
                    "META-INF/versions/*/module-info.class",
                )
                isPreserveFileTimestamps = false
                isReproducibleFileOrder = true
                failOnDuplicateEntries.set(true)
            }
            registry
        } else {
            null
        }
    val java = project.extensions.getByType(JavaPluginExtension::class.java)
    val launcher = project.extensions.getByType(JavaToolchainService::class.java).launcherFor(java.toolchain)
    val descriptor =
        project.tasks.register("chunkModule", WriteChunkModule::class.java) {
            app.set(appId)
            registry?.let { sessionCatalog.set(it.flatMap { task -> task.catalogFile }) }
            projectPath.set(project.path)
            javaVersion.set(java.toolchain.languageVersion.map { it.asInt() })
            javaExecutable.set(launcher.map { it.executablePath.asFile.absolutePath })
            jarFile.set(
                project.tasks.named(if (appId.isEmpty()) "jar" else "shadowJar", Jar::class.java).flatMap {
                    it.archiveFile
                },
            )
            outputFile.set(project.layout.buildDirectory.file("chunk/module.json"))
            dependsOn(project.tasks.named("validateChunkJvm"))
        }
    project.rootProject.tasks.named("chunkArtifacts", WriteChunkArtifacts::class.java) {
        modules.from(descriptor.flatMap { it.outputFile })
    }
}

internal data class ModuleArtifact(
    val app: String,
    val projectPath: String,
    val jar: String,
    val javaVersion: Int,
    val javaExecutable: String,
    val sessions: List<String>,
)
