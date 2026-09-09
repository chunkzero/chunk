plugins {
    id("chunk.kotlin-conventions")
    application
}

kotlin {
    jvmToolchain(25)
    compilerOptions { jvmTarget = org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_25 }
}

dependencies { implementation(project(":jvm:runtime")) }

application { mainClass = "dev.chunkzero.runtime.BridgeMainKt" }
