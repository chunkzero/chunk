plugins {
    id("chunk.java-conventions")
    id("org.jetbrains.kotlin.jvm")
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
    api(project(":jvm:runtime"))
    api(libs.minestom)
    implementation(project(":jvm:proto"))
    implementation(libs.grpc.netty)
    runtimeOnly(libs.slf4j.simple)
    testImplementation(kotlin("stdlib"))
}
