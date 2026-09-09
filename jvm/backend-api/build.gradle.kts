plugins {
    id("chunk.java-conventions")
}

dependencies {
    api(libs.gson)
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
