package dev.chunkzero.gradle

import org.gradle.api.Project
import org.gradle.api.tasks.SourceSetContainer
import org.gradle.api.tasks.compile.JavaCompile

internal fun configureComponents(
    project: Project,
    appId: String,
) {
    val sources = project.extensions.getByType(SourceSetContainer::class.java).named("main")
    val indexTask =
        project.tasks.register("generateChunkComponentIndex", WriteComponentIndex::class.java) {
            classes.from(sources.map { it.output.classesDirs })
            outputDirectory.set(project.layout.buildDirectory.dir("generated/chunk/component-index"))
            dependsOn(project.tasks.named("compileJava"))
            project.plugins.withId("org.jetbrains.kotlin.jvm") { dependsOn(project.tasks.named("compileKotlin")) }
        }
    sources.configure { resources.srcDir(indexTask.flatMap { it.outputDirectory }) }
    if (appId.isEmpty()) return
    val bindings =
        project.tasks.register("generateChunkComponentBindings", WriteComponentBindings::class.java) {
            app.set(appId)
            classes.from(sources.map { it.output.classesDirs })
            index.from(indexTask.flatMap { it.outputDirectory })
            // Resolve module JARs, whose resource indexes are built after their own compilation.
            dependencies.from(project.configurations.named("runtimeClasspath"))
            sourceDirectory.set(project.layout.buildDirectory.dir("generated/chunk/component-bindings"))
            resourceDirectory.set(project.layout.buildDirectory.dir("generated/chunk/component-services"))
        }
    val compile =
        project.tasks.register("compileChunkComponents", JavaCompile::class.java) {
            source(bindings.flatMap { it.sourceDirectory })
            classpath =
                project.files(sources.map { it.output.classesDirs }, project.configurations.named("compileClasspath"))
            destinationDirectory.set(project.layout.buildDirectory.dir("classes/chunkComponents/main"))
            javaCompiler.set(project.tasks.named("compileJava", JavaCompile::class.java).flatMap { it.javaCompiler })
        }
    sources.configure {
        resources.srcDir(bindings.flatMap { it.resourceDirectory })
        output.dir(mapOf("builtBy" to compile), compile.flatMap { it.destinationDirectory })
    }
}
