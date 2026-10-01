plugins {
    id("chunk.publishing-conventions")
    id("chunk.java-conventions")
}

dependencies {
    api(libs.jackson)
}

val generatedFixtures = layout.buildDirectory.dir("generated/contracts")
val generateTestContracts =
    tasks.register<Exec>("generateTestContracts") {
        workingDir(rootProject.projectDir)
        inputs.file("src/test/resources/contract.json")
        inputs.dir(rootProject.file("crates/chunk-build/src"))
        outputs.dir(generatedFixtures)
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
            file("src/test/resources/contract.json"),
            generatedFixtures.get().asFile,
            "dev.chunkzero.generated",
        )
    }
sourceSets.test {
    java.srcDir(generatedFixtures.map { it.dir("java") })
}
tasks.compileTestJava {
    dependsOn(generateTestContracts)
}
tasks.test {
    environment("CHUNK_ENVIRONMENT_NAME", "prod")
    systemProperty("chunk.root", rootProject.projectDir.absolutePath)
}
