import org.gradle.api.DefaultTask
import org.gradle.api.file.DirectoryProperty
import org.gradle.api.provider.Property
import org.gradle.api.tasks.Input
import org.gradle.api.tasks.InputDirectory
import org.gradle.api.tasks.Internal
import org.gradle.api.tasks.OutputDirectory
import org.gradle.api.tasks.PathSensitive
import org.gradle.api.tasks.PathSensitivity
import org.gradle.api.tasks.TaskAction
import org.gradle.process.ExecOperations
import javax.inject.Inject

abstract class GenerateBackend
    @Inject
    constructor(
        private val exec: ExecOperations,
    ) : DefaultTask() {
        @get:InputDirectory
        @get:PathSensitive(PathSensitivity.RELATIVE)
        abstract val backendProject: DirectoryProperty

        @get:Input
        abstract val packageName: Property<String>

        @get:Input
        abstract val moduleName: Property<String>

        @get:Internal
        abstract val platformDirectory: DirectoryProperty

        @get:OutputDirectory
        abstract val outputDirectory: DirectoryProperty

        @TaskAction
        fun generate() {
            require(moduleName.get().matches(Regex(":[A-Za-z0-9_-]+(?::[A-Za-z0-9_-]+)*")))
            val output = outputDirectory.get().asFile
            exec.exec {
                workingDir(platformDirectory.get().asFile)
                commandLine("node", "scripts/install-typescript.mjs")
            }
            exec.exec {
                workingDir(platformDirectory.get().asFile)
                commandLine(
                    "cargo",
                    "run",
                    "-q",
                    "-p",
                    "chunk-build",
                    "--bin",
                    "chunk-compile",
                    "--",
                    backendProject.get().asFile,
                    output.resolve("backend"),
                )
            }
            exec.exec {
                workingDir(platformDirectory.get().asFile)
                commandLine(
                    "cargo",
                    "run",
                    "-q",
                    "-p",
                    "chunk-build",
                    "--bin",
                    "chunk-codegen",
                    "--",
                    "java",
                    output.resolve("backend/contract.json"),
                    output.resolve("client"),
                    packageName.get(),
                )
            }
            output.resolve("backend/gameplay-module.txt").writeText(moduleName.get())
        }
    }
