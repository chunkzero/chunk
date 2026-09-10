plugins {
    id("chunk.kotlin-conventions")
    application
}

kotlin {
    jvmToolchain(25)
    compilerOptions { jvmTarget = org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_25 }
}

dependencies {
    implementation(project(":jvm:proto"))
    api(libs.minestom)
    api(project(":jvm:backend-client"))
    implementation(libs.grpc.netty)
    runtimeOnly(libs.slf4j.simple)
}

application { mainClass = "dev.chunkzero.runtime.BridgeMainKt" }
