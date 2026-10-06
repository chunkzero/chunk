plugins {
    id("chunk.publishing-conventions")
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
    api(project(":jvm:backend-client"))
    api(libs.multistom)
    api(libs.polar)
    implementation(project(":jvm:proto"))
    testImplementation(libs.grpc.netty)
    runtimeOnly(libs.slf4j.simple)
    testImplementation(kotlin("stdlib"))
}

dokka {
    dokkaSourceSets.configureEach {
        suppressedFiles.from(
            fileTree("src/main/java/com/chunkzero/chunk/runtime") {
                include("ManagedPlayer.java", "SessionManager.java", "SessionRegistration.java", "TickExecutor.java")
            },
        )
    }
}
