plugins {
    id("com.chunkzero.chunk")
}

java { toolchain.languageVersion = JavaLanguageVersion.of(25) }
tasks.withType<JavaCompile>().configureEach {
    options.compilerArgs.addAll(listOf("-Xlint:all", "-Werror"))
}

dependencies {
    testImplementation(platform(libs.junit.bom))
    testImplementation(libs.junit.jupiter)
    testRuntimeOnly(libs.junit.platform.launcher)
}

tasks.test { useJUnitPlatform() }

application { mainClass = "example.arena.Arena" }
