pluginManagement {
    includeBuild("${providers.gradleProperty("chunk.source").get()}/jvm/gradle-plugin") {
        name = "chunk-gradle-plugin"
    }
    repositories {
        gradlePluginPortal()
        mavenCentral()
    }
}

plugins {
    id("dev.chunkzero.chunk.settings")
    id("org.gradle.toolchains.foojay-resolver-convention") version "1.0.0"
}

dependencyResolutionManagement {
    repositories { mavenCentral() }
}

includeBuild(providers.gradleProperty("chunk.source").get()) { name = "chunk-platform" }
rootProject.name = rootDir.name
