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
    if (appId.isNotEmpty()) {
        project.pluginManager.apply("application")
        project.pluginManager.apply("com.gradleup.shadow")
        val app = project.configuration().apps.single { it.id == appId }
        val application = project.extensions.getByType(JavaApplication::class.java)
        val sources = project.extensions.getByType(SourceSetContainer::class.java).named("main")
        val manifest =
            project.tasks.register("generateChunkAppManifest", WriteAppManifest::class.java) {
                this.app.set(appId)
                mainClass.set(application.mainClass)
                machineProfile.set(app.machineProfile)
                capacity.set(app.capacity)
                classes.from(sources.map { it.output.classesDirs })
                dependencies.from(project.configurations.named("compileClasspath"))
                dependsOn(project.tasks.named("compileJava"))
                project.plugins.withId("org.jetbrains.kotlin.jvm") { dependsOn(project.tasks.named("compileKotlin")) }
                outputDirectory.set(project.layout.buildDirectory.dir("generated/chunk/app-resources"))
            }
        sources.configure { resources.srcDir(manifest.flatMap { it.outputDirectory }) }
        project.tasks.named("shadowJar", ShadowJar::class.java) {
            mergeServiceFiles()
            filesMatching(listOf("META-INF/services/**", "META-INF/*.kotlin_module")) {
                duplicatesStrategy =
                    DuplicatesStrategy.INCLUDE
            }
            filesMatching(listOf("**/*.class", "META-INF/chunk/app.json")) {
                duplicatesStrategy =
                    DuplicatesStrategy.FAIL
            }
            exclude(
                "META-INF/services/dev.chunkzero.runtime.SessionProvider",
                "module-info.class",
                "META-INF/versions/*/module-info.class",
            )
            isPreserveFileTimestamps = false
            isReproducibleFileOrder = true
            failOnDuplicateEntries.set(true)
        }
    }
    val java = project.extensions.getByType(JavaPluginExtension::class.java)
    val launcher = project.extensions.getByType(JavaToolchainService::class.java).launcherFor(java.toolchain)
    val descriptor =
        project.tasks.register("chunkModule", WriteChunkModule::class.java) {
            app.set(appId)
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
)
