pluginManagement {
    providers.gradleProperty("chunk.source").orNull?.let { source ->
        includeBuild("$source/jvm/gradle-plugin") { name = "chunk-gradle-plugin" }
    }
    repositories {
        maven {
            url = uri(providers.gradleProperty("chunk.mavenRepository").orElse("https://maven.chunkzero.com").get())
            isAllowInsecureProtocol = url.scheme == "http"
        }
        gradlePluginPortal()
        mavenCentral()
    }
}

plugins {
    id("dev.chunkzero.chunk.settings") version "@CHUNK_VERSION@"
    id("org.gradle.toolchains.foojay-resolver-convention") version "@FOOJAY_VERSION@"
}

dependencyResolutionManagement {
    repositories {
        maven {
            url = uri(providers.gradleProperty("chunk.mavenRepository").orElse("https://maven.chunkzero.com").get())
            isAllowInsecureProtocol = url.scheme == "http"
        }
        mavenCentral()
    }
}

providers.gradleProperty("chunk.source").orNull?.let { source ->
    includeBuild(source) { name = "chunk-platform" }
}
rootProject.name = "@PROJECT_NAME@"
