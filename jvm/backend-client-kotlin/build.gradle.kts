plugins { id("chunk.kotlin-conventions") }

dependencies {
    api(project(":jvm:backend-client"))
    api(libs.kotlinx.coroutines)
    testImplementation(libs.grpc.netty)
    testImplementation(project(":jvm:proto"))
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
            "kotlin",
            contract,
            generatedFixtures.get().asFile,
            "dev.chunkzero.generated",
        )
    }
sourceSets.test {
    java.srcDir(generatedFixtures.map { it.dir("java") })
    java.srcDir(generatedFixtures.map { it.dir("java-client") })
}
kotlin.sourceSets.test {
    kotlin.srcDir(generatedFixtures.map { it.dir("kotlin") })
}
tasks.compileTestKotlin {
    dependsOn(generateTestContracts)
}
tasks.compileTestJava {
    dependsOn(generateTestContracts)
}
