plugins {
    id("chunk.kotlin-conventions")
    `java-gradle-plugin`
}

dependencies {
    implementation(project(":jvm:build-api"))
}

gradlePlugin {
    plugins {
        create("chunk") {
            id = "dev.chunkzero.chunk"
            implementationClass = "dev.chunkzero.gradle.ChunkPlugin"
        }
    }
}
