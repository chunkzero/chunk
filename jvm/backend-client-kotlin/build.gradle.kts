plugins {
    id("chunk.kotlin-conventions")
    id("chunk.publishing-conventions")
}

dependencies {
    api(project(":jvm:backend-client"))
    api(libs.kotlinx.coroutines)
    testImplementation(libs.grpc.netty)
    testImplementation(project(":jvm:proto"))
}

val generateTestContracts =
    tasks.register<GenerateTestContracts>("generateTestContracts") {
        contract = project(":jvm:backend-api").layout.projectDirectory.file("src/test/resources/contract.json")
        language = ContractLanguage.KOTLIN
    }
sourceSets.test {
    java.srcDir(generateTestContracts.flatMap { it.outputDirectory.dir("java") })
    java.srcDir(generateTestContracts.flatMap { it.outputDirectory.dir("java-client") })
}
kotlin.sourceSets.test {
    kotlin.srcDir(generateTestContracts.flatMap { it.outputDirectory.dir("kotlin") })
}
