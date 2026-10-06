plugins {
    id("com.chunkzero.chunk")
}

java { toolchain.languageVersion = JavaLanguageVersion.of(25) }
tasks.withType<JavaCompile>().configureEach {
    options.compilerArgs.addAll(listOf("-Xlint:all", "-Werror"))
}

dependencies { implementation("com.chunkzero.chunk:multistom:${libs.versions.chunk.get()}") }

application { mainClass = "example.lobby.Lobby" }
