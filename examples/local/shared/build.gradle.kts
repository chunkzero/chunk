plugins {
    id("com.chunkzero.chunk.kotlin")
}

java { toolchain.languageVersion = JavaLanguageVersion.of(25) }
kotlin { compilerOptions { allWarningsAsErrors = true } }

dependencies {
    testImplementation("com.chunkzero.chunk:proto:${libs.versions.chunk.get()}")
    testImplementation(platform(libs.junit.bom))
    testImplementation(libs.junit.jupiter)
    testRuntimeOnly(libs.junit.platform.launcher)
}

tasks.test { useJUnitPlatform() }
