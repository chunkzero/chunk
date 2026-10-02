import org.gradle.api.DefaultTask
import org.gradle.api.file.ConfigurableFileCollection
import org.gradle.api.file.DirectoryProperty
import org.gradle.api.file.RegularFileProperty
import org.gradle.api.provider.Property
import org.gradle.api.tasks.Input
import org.gradle.api.tasks.InputFile
import org.gradle.api.tasks.InputFiles
import org.gradle.api.tasks.Internal
import org.gradle.api.tasks.OutputDirectory
import org.gradle.api.tasks.PathSensitive
import org.gradle.api.tasks.PathSensitivity
import org.gradle.api.tasks.TaskAction
import org.gradle.process.ExecOperations
import javax.inject.Inject

enum class ContractLanguage(
    val argument: String,
) {
    JAVA("java"),
    KOTLIN("kotlin"),
}

/** Runs `chunk-codegen` over a contract to produce test fixtures. */
abstract class GenerateTestContracts
    @Inject
    constructor(
        private val exec: ExecOperations,
    ) : DefaultTask() {
        @get:InputFile
        @get:PathSensitive(PathSensitivity.NONE)
        abstract val contract: RegularFileProperty

        @get:Input
        abstract val language: Property<ContractLanguage>

        @get:Input
        abstract val packageName: Property<String>

        @get:InputFiles
        @get:PathSensitive(PathSensitivity.RELATIVE)
        abstract val generatorSources: ConfigurableFileCollection

        @get:OutputDirectory
        abstract val outputDirectory: DirectoryProperty

        @get:Internal
        abstract val workingDirectory: DirectoryProperty

        init {
            val root = project.rootProject.layout.projectDirectory
            workingDirectory.convention(root)
            generatorSources.from(
                root.file("Cargo.toml"),
                root.file("Cargo.lock"),
                root.file("crates/chunk-build/Cargo.toml"),
                root.dir("crates/chunk-build/src"),
                root.file("crates/chunk-contract/Cargo.toml"),
                root.dir("crates/chunk-contract/src"),
            )
            outputDirectory.convention(project.layout.buildDirectory.dir("generated/contracts"))
            packageName.convention("dev.chunkzero.generated")
        }

        @TaskAction
        fun generate() {
            val root = workingDirectory.get().asFile
            val target = language.get().argument
            val input = contract.get().asFile
            val output = outputDirectory.get().asFile
            val javaPackage = packageName.get()
            exec.exec {
                workingDir(root)
                commandLine(
                    "cargo",
                    "run",
                    "-q",
                    "-p",
                    "chunk-build",
                    "--bin",
                    "chunk-codegen",
                    "--",
                    target,
                    input,
                    output,
                    javaPackage,
                )
            }
        }
    }
