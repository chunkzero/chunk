import dev.chunkzero.gradle.GenerateChunkBackend

plugins {
    id("dev.chunkzero.chunk.kotlin")
}

java { toolchain.languageVersion = JavaLanguageVersion.of(25) }
kotlin { compilerOptions { allWarningsAsErrors = true } }

dependencies {
    testImplementation(platform(libs.junit.bom))
    testImplementation(libs.junit.jupiter)
    testRuntimeOnly(libs.junit.platform.launcher)
}

val generate = rootProject.tasks.named<GenerateChunkBackend>("generateChunkBackend")
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
