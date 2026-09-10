import dev.chunkzero.gradle.GenerateChunkBackend

plugins {
    id("dev.chunkzero.chunk.kotlin")
    application
}

java { toolchain.languageVersion = JavaLanguageVersion.of(25) }
kotlin { compilerOptions { allWarningsAsErrors = true } }

val appJars by configurations.creating {
    isCanBeConsumed = false
    isTransitive = false
}

dependencies {
    appJars(project(path = ":apps:lobby", configuration = "runtimeElements"))
    appJars(project(path = ":apps:arena", configuration = "runtimeElements"))
    testImplementation(platform(libs.junit.bom))
    testImplementation(libs.junit.jupiter)
    testRuntimeOnly(libs.junit.platform.launcher)
}

application {
    mainClass = "dev.chunkzero.runtime.BridgeMain"
    applicationName = "example"
}
distributions.main { distributionBaseName = "example" }
tasks.startScripts { classpath = files(classpath, appJars) }

abstract class WriteExampleFile : DefaultTask() {
    @get:Input
    abstract val content: Property<String>

    @get:OutputFile
    abstract val outputFile: RegularFileProperty

    @TaskAction
    fun write() {
        val output = outputFile.get().asFile
        output.parentFile.mkdirs()
        output.writeText(content.get())
    }
}

val localJava = javaToolchains.launcherFor(java.toolchain)
tasks.register<WriteExampleFile>("writeJavaExecutable") {
    content.set(localJava.map { it.executablePath.asFile.absolutePath })
    outputFile.set(layout.buildDirectory.file("java-executable.txt"))
}
val moduleMarker =
    tasks.register<WriteExampleFile>("writeGameplayModule") {
        content.set(project.path)
        outputFile.set(layout.buildDirectory.file("compatibility/gameplay-module.txt"))
    }
val generate = rootProject.tasks.named<GenerateChunkBackend>("generateChunkBackend")
distributions.main {
    contents {
        from(appJars) { into("lib") }
        from(generate.flatMap { it.backendDirectory }) {
            include("backend.json", "contract.json", "source.mjs", "source.mjs.map")
            into("backend")
        }
        from(moduleMarker) { into("backend") }
    }
}

val platformDirectory = rootProject.file("../..")
val buildBackendExecutable =
    tasks.register<Exec>("buildBackendExecutable") {
        workingDir(platformDirectory)
        inputs.files(
            fileTree(platformDirectory.resolve("crates")) { include("**/src/**", "**/Cargo.toml", "**/build.rs") },
        )
        inputs.files(
            listOf("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "mise.toml").map(platformDirectory::resolve),
        )
        inputs.dir(platformDirectory.resolve("proto"))
        outputs.file(platformDirectory.resolve("target/debug/chunk-backend"))
        commandLine("cargo", "build", "-q", "-p", "chunk-backend")
    }
tasks.test {
    useJUnitPlatform()
    dependsOn(buildBackendExecutable, generate)
    inputs.files(buildBackendExecutable)
    inputs.dir(generate.flatMap { it.backendDirectory })
    systemProperty("chunk.executable", platformDirectory.resolve("target/debug/chunk-backend").absolutePath)
    systemProperty("chunk.backend", rootProject.file(".chunk/build/backend").absolutePath)
}
