plugins {
    id("chunk.kotlin-conventions")
    id("chunk.publishing-conventions")
}

kotlin {
    jvmToolchain(25)
    compilerOptions { jvmTarget = org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_25 }
}

dependencies {
    api(project(":jvm:runtime-minestom"))
    api(project(":jvm:backend-client-kotlin"))
    testImplementation(project(":jvm:proto"))
    testImplementation(libs.grpc.netty)
}
