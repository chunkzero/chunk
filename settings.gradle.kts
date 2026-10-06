pluginManagement {
    includeBuild("jvm/gradle-plugin") { name = "chunk-gradle-plugin" }
    repositories {
        gradlePluginPortal()
        mavenCentral()
    }
}

plugins {
    id("org.gradle.toolchains.foojay-resolver-convention") version "1.0.0"
}

dependencyResolutionManagement {
    repositoriesMode = RepositoriesMode.FAIL_ON_PROJECT_REPOS
    repositories {
        maven("https://maven.chunkzero.com/nightlies") {
            mavenContent { includeGroup("com.chunkzero.multistom") }
        }
        mavenCentral()
    }
}

rootProject.name = "chunk"

include(":jvm:proto")
include(":jvm:backend-api")
include(":jvm:backend-client")
include(":jvm:backend-client-kotlin")
include(":jvm:runtime")
include(":jvm:multistom")
include(":jvm:multistom-kotlin")
include(":jvm:minestom")
include(":jvm:world-converter")
