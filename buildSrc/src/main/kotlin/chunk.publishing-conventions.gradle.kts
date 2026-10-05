import org.jetbrains.dokka.gradle.engine.parameters.VisibilityModifier
import org.jetbrains.dokka.gradle.tasks.DokkaGeneratePublicationTask

plugins {
    `java-library`
    `maven-publish`
    id("org.jetbrains.dokka")
}

tasks.register<Jar>("javadocJar") {
    archiveClassifier.set("javadoc")
    from(tasks.named<DokkaGeneratePublicationTask>("dokkaGeneratePublicationHtml").flatMap { it.outputDirectory })
}

java {
    withSourcesJar()
    withJavadocJar()
}

dokka {
    dokkaSourceSets.configureEach {
        jdkVersion.set(java.toolchain.languageVersion.map { it.asInt() })
        documentedVisibilities.set(setOf(VisibilityModifier.Public, VisibilityModifier.Protected))
        perPackageOption {
            matchingRegex.set(".*\\.internal(\\..*)?")
            suppress.set(true)
        }
        perPackageOption {
            matchingRegex.set("com\\.chunkzero\\.chunk\\.runtime\\.(bootstrap|control)(\\..*)?")
            suppress.set(true)
        }
    }
}

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
