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

val generateTestContracts =
    tasks.register<GenerateTestContracts>("generateTestContracts") {
        contract = project(":jvm:backend-api").layout.projectDirectory.file("src/test/resources/contract.json")
        language = ContractLanguage.JAVA
    }
sourceSets.test {
    java.srcDir(generateTestContracts.flatMap { it.outputDirectory.dir("java") })
    java.srcDir(generateTestContracts.flatMap { it.outputDirectory.dir("java-client") })
}
