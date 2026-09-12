plugins {
    `java-library`
    `maven-publish`
}

java { withSourcesJar() }

tasks.withType<AbstractArchiveTask>().configureEach {
    isPreserveFileTimestamps = false
    isReproducibleFileOrder = true
}

publishing {
    publications {
        create<MavenPublication>("sdk") {
            from(components["java"])
            pom {
                name = "Chunk ${project.name}"
                description = "Minecraft application runtime and SDK"
                url = "https://github.com/chunkzero/chunk"
                licenses {
                    license {
                        name = "FSL-1.1-MIT"
                        url = "https://github.com/chunkzero/chunk/blob/main/LICENSE.md"
                    }
                }
            }
        }
    }
    repositories {
        maven {
            name = "sdk"
            url =
                uri(
                    providers.gradleProperty("chunk.sdkRepository").orElse(
                        rootProject.layout.buildDirectory
                            .dir("sdk/maven")
                            .map { it.asFile.absolutePath },
                    ),
                )
        }
    }
}
