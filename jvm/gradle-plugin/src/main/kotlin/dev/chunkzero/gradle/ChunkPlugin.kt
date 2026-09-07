package dev.chunkzero.gradle

import org.gradle.api.Plugin
import org.gradle.api.Project

/**
 * Registered scaffold for chunk's Gradle integration.
 *
 * Will wire generated clients, framework dependencies, server JAR and asset
 * builds into the chunk toolchain. No application build wiring exists yet.
 */
class ChunkPlugin : Plugin<Project> {
    override fun apply(target: Project) {
        // Wiring arrives with the first build step.
    }
}
