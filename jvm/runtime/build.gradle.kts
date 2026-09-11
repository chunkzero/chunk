plugins {
    id("chunk.java-conventions")
    id("org.jetbrains.kotlin.jvm")
    application
}

kotlin {
    jvmToolchain(25)
    compilerOptions {
        jvmTarget = org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_25
        allWarningsAsErrors = true
    }
}

java { toolchain.languageVersion = JavaLanguageVersion.of(25) }
tasks.withType<JavaCompile>().configureEach { options.release = 25 }

dependencies {
    implementation(project(":jvm:proto"))
    api(libs.minestom)
    api(project(":jvm:backend-client"))
    implementation(libs.grpc.netty)
    runtimeOnly(libs.slf4j.simple)
    testImplementation(kotlin("stdlib"))
}

application { mainClass = "dev.chunkzero.runtime.BridgeMain" }
