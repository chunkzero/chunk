plugins {
    id("dev.chunkzero.chunk.kotlin")
}

java { toolchain.languageVersion = JavaLanguageVersion.of(25) }
kotlin { compilerOptions { allWarningsAsErrors = true } }

application { mainClass = "dev.chunkzero.example.load.LobbyKt" }
