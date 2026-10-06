package com.chunkzero.chunk.gradle

import org.gradle.api.Plugin
import org.gradle.api.Project
import org.gradle.api.plugins.JavaLibraryPlugin
import org.gradle.api.plugins.JavaPluginExtension
import org.gradle.jvm.tasks.Jar
import org.jetbrains.kotlin.gradle.dsl.JvmTarget
import org.jetbrains.kotlin.gradle.dsl.KotlinJvmProjectExtension
import org.jetbrains.kotlin.gradle.tasks.KotlinJvmCompile

class ChunkKotlinPlugin : Plugin<Project> {
    override fun apply(project: Project) {
        project.pluginManager.apply(ChunkPlugin::class.java)
        if (project == project.rootProject) {
            project.tasks.named("generateChunkBackend", GenerateChunkBackend::class.java) { target.set("kotlin") }
            project.project(KOTLIN_BINDINGS).pluginManager.apply(KotlinBindingsPlugin::class.java)
        } else {
            check(project.rootProject.plugins.hasPlugin(ChunkKotlinPlugin::class.java)) {
                "Apply com.chunkzero.chunk.kotlin to the root project to enable the shared Kotlin facade"
            }
            applyKotlin(project)
            project.dependencies.add("implementation", project.dependencies.project(mapOf("path" to KOTLIN_BINDINGS)))
            project.dependencies.add("implementation", framework("multistom-kotlin"))
        }
    }
}

internal class KotlinBindingsPlugin : Plugin<Project> {
    override fun apply(project: Project) {
        project.pluginManager.apply(JavaLibraryPlugin::class.java)
        val rootJava = project.rootProject.extensions.getByType(JavaPluginExtension::class.java)
        project.extensions
            .getByType(
                JavaPluginExtension::class.java,
            ).toolchain.languageVersion
            .set(rootJava.toolchain.languageVersion)
        configureJava(project)
        applyKotlin(project)
        val generate = project.rootProject.tasks.named("generateChunkBackend", GenerateChunkBackend::class.java)
        project.extensions.getByType(KotlinJvmProjectExtension::class.java).sourceSets.named("main") {
            kotlin.srcDir(generate.flatMap { it.generatedDirectory.dir("kotlin") })
        }
        project.tasks.named("compileKotlin") { dependsOn(generate) }
        project.tasks.named("jar", Jar::class.java) { archiveBaseName.set("chunk-backend-kotlin") }
        project.dependencies.add("api", project.dependencies.project(mapOf("path" to ":")))
        project.dependencies.add("api", framework("backend-client-kotlin"))
        configureModule(project, "")
    }
}

private fun applyKotlin(project: Project) {
    val compilationType =
        try {
            KotlinJvmCompile::class.java
        } catch (error: NoClassDefFoundError) {
            throw IllegalStateException(
                "Declare the Kotlin JVM plugin version with apply false in the settings plugins block",
                error,
            )
        }
    try {
        project.pluginManager.apply("org.jetbrains.kotlin.jvm")
    } catch (error: org.gradle.api.plugins.UnknownPluginException) {
        throw IllegalStateException(
            "Declare the Kotlin JVM plugin version with apply false in the settings plugins block",
            error,
        )
    }
    val java = project.extensions.getByType(JavaPluginExtension::class.java)
    val validation = project.tasks.named("validateChunkJvm", ValidateChunkJvm::class.java)
    validation.configure {
        project.tasks.withType(compilationType).forEach { compile ->
            targets.put(compile.name, compile.compilerOptions.jvmTarget.map { it.target.toInt() })
        }
    }
    project.tasks.withType(compilationType).configureEach {
        compilerOptions.jvmTarget.convention(
            java.toolchain.languageVersion.map { JvmTarget.fromTarget(it.asInt().toString()) },
        )
        dependsOn(validation)
    }
}
