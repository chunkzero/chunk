plugins {
    id("chunk.kotlin-conventions")
    `java-gradle-plugin`
}

gradlePlugin {
    plugins {
        create("chunk") {
            id = "dev.chunkzero.chunk"
            implementationClass = "dev.chunkzero.gradle.ChunkPlugin"
        }
    }
}
