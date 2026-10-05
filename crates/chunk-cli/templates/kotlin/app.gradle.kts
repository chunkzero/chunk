plugins {
    id("com.chunkzero.chunk.kotlin")
}

java { toolchain.languageVersion = JavaLanguageVersion.of(25) }
kotlin { compilerOptions { allWarningsAsErrors = true } }

application { mainClass = "example.LobbyKt" }
