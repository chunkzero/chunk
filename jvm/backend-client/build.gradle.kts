plugins {
    id("chunk.publishing-conventions")
    id("chunk.java-conventions")
}

dependencies {
    api(project(":jvm:backend-api"))
    api(libs.grpc.api)
    implementation(libs.grpc.stub)
    implementation(project(":jvm:proto"))
    testImplementation(project(":jvm:proto"))
    testImplementation(libs.grpc.netty)
}

val generatedFixtures = layout.buildDirectory.dir("generated/contracts")
val generateTestContracts =
    tasks.register<Exec>("generateTestContracts") {
        workingDir(rootProject.projectDir)
        val contract = project(":jvm:backend-api").file("src/test/resources/contract.json")
        inputs.file(contract)
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
            contract,
            generatedFixtures.get().asFile,
            "dev.chunkzero.generated",
        )
    }
sourceSets.test {
    java.srcDir(generatedFixtures.map { it.dir("java") })
    java.srcDir(generatedFixtures.map { it.dir("java-client") })
}
tasks.compileTestJava {
    dependsOn(generateTestContracts)
}
