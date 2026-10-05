plugins {
    id("com.chunkzero.chunk.kotlin")
}

java { toolchain.languageVersion = JavaLanguageVersion.of(25) }
kotlin { compilerOptions { allWarningsAsErrors = true } }

dependencies { implementation(project(":shared")) }

application { mainClass = "com.chunkzero.chunk.example.arena.ArenaSessionsKt" }
