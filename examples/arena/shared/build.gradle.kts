plugins {
    id("dev.chunkzero.chunk")
}

java { toolchain.languageVersion = JavaLanguageVersion.of(25) }
tasks.withType<JavaCompile>().configureEach {
    options.compilerArgs.addAll(listOf("-Xlint:all", "-Werror"))
}

dependencies {
    api("dev.hollowcube:polar:1.16.0")
    implementation("org.slf4j:slf4j-api:2.0.19")
}
