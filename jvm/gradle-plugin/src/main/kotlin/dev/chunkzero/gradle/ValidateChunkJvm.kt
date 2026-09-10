package dev.chunkzero.gradle

import org.gradle.api.DefaultTask
import org.gradle.api.provider.MapProperty
import org.gradle.api.provider.Property
import org.gradle.api.tasks.Input
import org.gradle.api.tasks.Optional
import org.gradle.api.tasks.TaskAction
import org.gradle.work.DisableCachingByDefault

@DisableCachingByDefault(because = "Validation has no outputs")
abstract class ValidateChunkJvm : DefaultTask() {
    @get:Input
    abstract val projectName: Property<String>

    @get:Input
    @get:Optional
    abstract val javaVersion: Property<Int>

    @get:Input
    abstract val targets: MapProperty<String, Int>

    @TaskAction
    fun validate() {
        val version =
            requireNotNull(javaVersion.orNull) {
                "${projectName.get()} must explicitly configure java.toolchain.languageVersion or a shared JVM convention"
            }
        require(version >= 25) { "${projectName.get()} requires JDK 25 or newer for the selected Chunk runtime" }
        targets.get().forEach { (task, target) ->
            require(
                target == version,
            ) { "${projectName.get()}:$task targets Java $target but the selected toolchain is $version" }
        }
    }
}
