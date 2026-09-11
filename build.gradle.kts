group = "dev.chunkzero"
version = libs.versions.chunk.get()

listOf("test", "assemble", "build").forEach { task ->
    tasks.register(task) { dependsOn(gradle.includedBuild("chunk-gradle-plugin").task(":$task")) }
}
