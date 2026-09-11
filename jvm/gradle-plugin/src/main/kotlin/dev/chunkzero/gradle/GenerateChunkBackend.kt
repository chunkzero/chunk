package dev.chunkzero.gradle

import org.gradle.api.DefaultTask
import org.gradle.api.file.DirectoryProperty
import org.gradle.api.provider.Property
import org.gradle.api.tasks.Input
import org.gradle.api.tasks.Internal
import org.gradle.api.tasks.OutputDirectory
import org.gradle.api.tasks.TaskAction
import org.gradle.process.ExecOperations
import org.gradle.work.DisableCachingByDefault
import javax.inject.Inject

@DisableCachingByDefault(because = "The compiler maintains its own materialized SDK inputs")
abstract class GenerateChunkBackend : DefaultTask() {
    @get:Inject
    abstract val exec: ExecOperations

    @get:Internal
    abstract val projectDirectory: DirectoryProperty

    @get:Input
    abstract val executable: Property<String>

    @get:Input
    abstract val javaPackage: Property<String>

    @get:Input
    abstract val target: Property<String>

    @get:OutputDirectory
    abstract val backendDirectory: DirectoryProperty

    @get:OutputDirectory
    abstract val generatedDirectory: DirectoryProperty

    @TaskAction
    fun generate() {
        exec.exec {
            workingDir(projectDirectory.get().asFile)
            commandLine(
                this@GenerateChunkBackend.executable.get(),
                "gen",
                projectDirectory.get().asFile,
                "--target",
                target.get(),
                "--java-package",
                javaPackage.get(),
                "--output",
                generatedDirectory.get().asFile,
                "--backend-output",
                backendDirectory.get().asFile,
            )
        }
    }
}
