import com.chunkzero.chunk.gradle.ChunkSettingsExtension

pluginManagement {
    includeBuild("../../jvm/gradle-plugin") { name = "chunk-gradle-plugin" }
    repositories {
        gradlePluginPortal()
        mavenCentral()
    }
}

plugins {
    id("com.chunkzero.chunk.settings")
    id("org.gradle.toolchains.foojay-resolver-convention") version "1.0.0"
}

extensions.configure<ChunkSettingsExtension> {
    executable.set(file("../../target/debug/chunk").absolutePath)
}

dependencyResolutionManagement {
    repositories {
        maven("https://maven.chunkzero.com/nightlies") {
            mavenContent { includeGroup("com.chunkzero.multistom") }
        }
        mavenCentral()
    }
    versionCatalogs { create("libs") { from(files("../../gradle/libs.versions.toml")) } }
}

includeBuild("../..") { name = "chunk-platform" }
rootProject.name = "arena"
