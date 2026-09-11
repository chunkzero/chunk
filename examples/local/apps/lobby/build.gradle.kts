plugins {
    id("dev.chunkzero.chunk.kotlin")
}

java { toolchain.languageVersion = JavaLanguageVersion.of(25) }
kotlin { compilerOptions { allWarningsAsErrors = true } }

dependencies { implementation(project(":shared")) }

application { mainClass = "dev.chunkzero.example.lobby.LobbySessionsKt" }
