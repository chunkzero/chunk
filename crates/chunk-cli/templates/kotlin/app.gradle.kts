plugins {
    id("org.jetbrains.kotlin.jvm")
    id("com.chunkzero.chunk")
}

kotlin {
    jvmToolchain(25)
    compilerOptions { allWarningsAsErrors = true }
}

dependencies { implementation("com.chunkzero.chunk:multistom-kotlin:@CHUNK_VERSION@") }

application { mainClass = "example.LobbyKt" }
