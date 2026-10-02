plugins {
    id("chunk.publishing-conventions")
    id("chunk.java-conventions")
}

dependencies {
    api(libs.jackson)
}

val generateTestContracts =
    tasks.register<GenerateTestContracts>("generateTestContracts") {
        contract = layout.projectDirectory.file("src/test/resources/contract.json")
        language = ContractLanguage.JAVA
    }
sourceSets.test {
    java.srcDir(generateTestContracts.flatMap { it.outputDirectory.dir("java") })
}
tasks.test {
    environment("CHUNK_ENVIRONMENT_NAME", "prod")
    systemProperty("chunk.root", rootProject.projectDir.absolutePath)
}
