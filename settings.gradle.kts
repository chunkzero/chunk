pluginManagement {
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
        mavenCentral()
    }
}

rootProject.name = "chunk"

include(":jvm:proto")
include(":jvm:backend-client")
include(":jvm:backend-api")
include(":jvm:backend-java")
include(":jvm:runtime")
include(":jvm:build-api")
include(":jvm:gradle-plugin")
