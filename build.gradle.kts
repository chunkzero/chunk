group = "dev.chunkzero"
version = libs.versions.chunk.get()

listOf("test", "assemble", "build").forEach { task ->
    tasks.register(task) { dependsOn(gradle.includedBuild("chunk-gradle-plugin").task(":$task")) }
}

tasks.register("publishSdk") {
    description = "Publish the matching JVM SDK and Gradle plugins to a local Maven repository."
    dependsOn(
        subprojects
            .filter {
                it.plugins.hasPlugin("maven-publish")
            }.map { "${it.path}:publishAllPublicationsToSdkRepository" },
    )
    dependsOn(gradle.includedBuild("chunk-gradle-plugin").task(":publishAllPublicationsToSdkRepository"))
}
