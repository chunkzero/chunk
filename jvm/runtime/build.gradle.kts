plugins {
    id("chunk.kotlin-conventions")
    application
}

kotlin {
    jvmToolchain(25)
    compilerOptions { jvmTarget = org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_25 }
}

dependencies {
    api(project(":jvm:proto"))
    implementation(libs.minestom)
    implementation(libs.grpc.netty)
    runtimeOnly(libs.slf4j.simple)
}

application { mainClass = "dev.chunkzero.runtime.BridgeMainKt" }
