plugins {
    id("chunk.kotlin-conventions")
    id("chunk.backend-generation")
    application
}

kotlin {
    jvmToolchain(25)
    compilerOptions { jvmTarget = org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_25 }
}

dependencies { implementation(project(":jvm:runtime")) }

application { mainClass = "dev.chunkzero.runtime.BridgeMainKt" }

val localJava = javaToolchains.launcherFor { languageVersion = JavaLanguageVersion.of(25) }

abstract class WriteJavaExecutable : DefaultTask() {
    @get:Input
    abstract val executable: Property<String>

    @get:OutputFile
    abstract val outputFile: RegularFileProperty

    @TaskAction
    fun write() {
        outputFile.get().asFile.writeText(executable.get())
    }
}

tasks.register<WriteJavaExecutable>("writeJavaExecutable") {
    executable.set(localJava.map { it.executablePath.asFile.absolutePath })
    outputFile.set(layout.buildDirectory.file("java-executable.txt"))
}

tasks.named<GenerateBackend>("generateBackend") {
    backendProject.set(rootProject.layout.projectDirectory.dir("examples/local"))
    packageName.set("dev.chunkzero.example.generated")
}

val buildBackendExecutable =
    tasks.register<Exec>("buildBackendExecutable") {
        workingDir(rootProject.projectDir)
        inputs.files(rootProject.fileTree("crates") { include("**/src/**", "**/Cargo.toml") })
        inputs.files(rootProject.file("Cargo.toml"), rootProject.file("Cargo.lock"))
        inputs.dir(rootProject.file("proto"))
        outputs.file(rootProject.file("target/debug/chunk"))
        commandLine("cargo", "build", "-q", "-p", "chunk")
    }
tasks.test {
    dependsOn(buildBackendExecutable)
    systemProperty("chunk.executable", rootProject.file("target/debug/chunk").absolutePath)
    systemProperty(
        "chunk.backend",
        layout.buildDirectory
            .dir("generated/chunk/backend")
            .get()
            .asFile.absolutePath,
    )
}
