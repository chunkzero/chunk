import com.chunkzero.chunk.gradle.ChunkSettingsExtension

pluginManagement {
    includeBuild("../../jvm/gradle-plugin") { name = "chunk-gradle-plugin" }
    repositories {
        gradlePluginPortal()
        mavenCentral()
    }
}

plugins {
    id("org.jetbrains.kotlin.jvm") version "2.4.10" apply false
    id("com.chunkzero.chunk.settings")
    id("org.gradle.toolchains.foojay-resolver-convention") version "1.0.0"
}

extensions.configure<ChunkSettingsExtension> {
    executable.set(file("../../target/debug/chunk").absolutePath)
    javaPackage.set("com.chunkzero.chunk.example.load.generated")
}

dependencyResolutionManagement {
    repositories {
        maven("https://maven.chunkzero.com/nightlies") {
            mavenContent { includeGroup("com.chunkzero.multistom") }
        }
        mavenCentral()
    }
}

includeBuild("../..") { name = "chunk-platform" }
rootProject.name = "load"
