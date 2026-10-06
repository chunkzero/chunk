plugins {
    id("org.jetbrains.kotlin.jvm")
    id("com.chunkzero.chunk")
}

kotlin {
    jvmToolchain(25)
    compilerOptions { allWarningsAsErrors = true }
}

dependencies {
    implementation(project(":shared"))
    implementation("com.chunkzero.chunk:multistom-kotlin:${libs.versions.chunk.get()}")
}

application { mainClass = "com.chunkzero.chunk.example.arena.ArenaSessionsKt" }
