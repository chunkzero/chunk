plugins {
    id("org.jetbrains.kotlin.jvm")
    id("com.chunkzero.chunk")
}

kotlin {
    jvmToolchain(25)
    compilerOptions { allWarningsAsErrors = true }
}

dependencies {
    api("com.chunkzero.chunk:multistom-kotlin:${libs.versions.chunk.get()}")
    testImplementation("com.chunkzero.chunk:proto:${libs.versions.chunk.get()}")
    testImplementation(platform(libs.junit.bom))
    testImplementation(libs.junit.jupiter)
    testRuntimeOnly(libs.junit.platform.launcher)
}

tasks.test { useJUnitPlatform() }
