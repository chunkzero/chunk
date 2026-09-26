import dev.chunkzero.gradle.GenerateChunkBackend

plugins {
    id("dev.chunkzero.chunk.kotlin")
}

java { toolchain.languageVersion = JavaLanguageVersion.of(25) }
kotlin { compilerOptions { allWarningsAsErrors = true } }

dependencies {
    testImplementation("dev.chunkzero:proto:${libs.versions.chunk.get()}")
    testImplementation(platform(libs.junit.bom))
    testImplementation(libs.junit.jupiter)
    testRuntimeOnly(libs.junit.platform.launcher)
}

val generate = rootProject.tasks.named<GenerateChunkBackend>("generateChunkBackend")
val platformDirectory = rootProject.file("../..")
val buildEnvironmentExecutable =
    tasks.register<Exec>("buildEnvironmentExecutable") {
        workingDir(platformDirectory)
        inputs.files(
            fileTree(platformDirectory.resolve("crates")) { include("**/src/**", "**/Cargo.toml", "**/build.rs") },
        )
        inputs.files(
            listOf("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "mise.toml").map(platformDirectory::resolve),
        )
        inputs.dir(platformDirectory.resolve("proto"))
        outputs.file(platformDirectory.resolve("target/debug/chunk-environment"))
        commandLine("cargo", "build", "-q", "-p", "chunk-environment")
    }
tasks.test {
    useJUnitPlatform()
    dependsOn(buildEnvironmentExecutable, generate)
    inputs.files(buildEnvironmentExecutable)
    inputs.dir(generate.flatMap { it.backendDirectory })
    systemProperty("chunk.executable", platformDirectory.resolve("target/debug/chunk-environment").absolutePath)
    systemProperty("chunk.backend", rootProject.file(".chunk/build/backend").absolutePath)
}
