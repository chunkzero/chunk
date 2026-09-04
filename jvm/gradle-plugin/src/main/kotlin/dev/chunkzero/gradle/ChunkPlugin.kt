package dev.chunkzero.gradle

import org.gradle.api.Plugin
import org.gradle.api.Project

/**
 * `dev.chunkzero.chunk`: the one plugin line in an application's build file.
 *
 * Maps the platform layout (`edge/`, `src/`, `tests/`, `maps/`, `packs/`)
 * onto Gradle source sets, wires generated sources from `.chunk/generated/`,
 * runs build plugins through `chunk-build-api`, and exposes `block()` and
 * `overworld()` dependency helpers. The `chunk` binary drives this plugin;
 * developers run `chunk build`, not Gradle.
 */
class ChunkPlugin : Plugin<Project> {
    override fun apply(target: Project) {
        // Wiring arrives with the first build step.
    }
}
