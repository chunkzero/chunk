package com.chunkzero.chunk.gradle

import org.gradle.api.DefaultTask
import org.gradle.api.Project
import org.gradle.api.artifacts.component.ProjectComponentIdentifier
import org.gradle.api.attributes.Bundling
import org.gradle.api.attributes.Category
import org.gradle.api.attributes.LibraryElements
import org.gradle.api.attributes.Usage
import org.gradle.api.file.ConfigurableFileCollection
import org.gradle.api.file.DirectoryProperty
import org.gradle.api.file.RegularFileProperty
import org.gradle.api.plugins.JavaPluginExtension
import org.gradle.api.provider.ListProperty
import org.gradle.api.provider.Property
import org.gradle.api.tasks.CacheableTask
import org.gradle.api.tasks.Classpath
import org.gradle.api.tasks.Input
import org.gradle.api.tasks.InputDirectory
import org.gradle.api.tasks.Nested
import org.gradle.api.tasks.OutputFile
import org.gradle.api.tasks.PathSensitive
import org.gradle.api.tasks.PathSensitivity
import org.gradle.api.tasks.TaskAction
import org.gradle.jvm.toolchain.JavaLauncher
import org.gradle.jvm.toolchain.JavaToolchainService
import org.gradle.process.ExecOperations
import java.io.File
import javax.inject.Inject

/** Converts an Anvil save to Polar with Chunk's world converter on the app's runtime classpath. */
@CacheableTask
abstract class ConvertAnvilWorld : DefaultTask() {
    @get:InputDirectory
    @get:PathSensitive(PathSensitivity.RELATIVE)
    abstract val source: DirectoryProperty

    /** `[fromX, fromZ, toX, toZ]`, inclusive, or empty for the whole save. */
    @get:Input
    abstract val chunks: ListProperty<Int>

    @get:Classpath
    abstract val classpath: ConfigurableFileCollection

    @get:Nested
    abstract val launcher: Property<JavaLauncher>

    @get:OutputFile
    abstract val output: RegularFileProperty

    @get:Inject
    abstract val exec: ExecOperations

    @TaskAction
    fun convert() {
        exec.javaexec {
            executable =
                launcher
                    .get()
                    .executablePath.asFile.absolutePath
            classpath = this@ConvertAnvilWorld.classpath
            mainClass.set("com.chunkzero.chunk.worldconverter.AnvilConverter")
            jvmArgs("--enable-native-access=ALL-UNNAMED")
            args(source.get().asFile.absolutePath, output.get().asFile.absolutePath)
            args(chunks.get())
        }
    }
}

/**
 * Converts each of the app's Anvil worlds to `.chunk/build/worlds/<app>/<name>.polar`, as part of
 * `chunkArtifacts`. The converter, `com.chunkzero.chunk:world-converter` at the plugin's version,
 * runs on the app's resolved runtime classpath without the build's own projects, so it uses the
 * app's Minestom (upstream or multistom) and app code changes don't reconvert. Its Polar resolves
 * to the app's version when the app has one.
 */
internal fun configureWorlds(
    project: Project,
    directory: File,
    app: AppMetadata,
) {
    if (app.anvilWorlds.isEmpty()) return
    val runtimeClasspath = project.configurations.named("runtimeClasspath")
    val converter =
        project.configurations.detachedConfiguration(project.dependencies.create(framework("world-converter"))).apply {
            shouldResolveConsistentlyWith(runtimeClasspath.get())
            attributes {
                attribute(Usage.USAGE_ATTRIBUTE, project.objects.named(Usage::class.java, Usage.JAVA_RUNTIME))
                attribute(Category.CATEGORY_ATTRIBUTE, project.objects.named(Category::class.java, Category.LIBRARY))
                attribute(
                    LibraryElements.LIBRARY_ELEMENTS_ATTRIBUTE,
                    project.objects.named(LibraryElements::class.java, LibraryElements.JAR),
                )
                attribute(Bundling.BUNDLING_ATTRIBUTE, project.objects.named(Bundling::class.java, Bundling.EXTERNAL))
            }
        }
    val runtime =
        runtimeClasspath.map { configuration ->
            configuration.incoming
                .artifactView {
                    componentFilter { it !is ProjectComponentIdentifier || it.build.buildPath != ":" }
                }.files
        }
    val java = project.extensions.getByType(JavaPluginExtension::class.java)
    val launcher = project.extensions.getByType(JavaToolchainService::class.java).launcherFor(java.toolchain)
    app.anvilWorlds.forEach { world ->
        val conversion =
            project.tasks.register("convertChunkWorld_${world.name}", ConvertAnvilWorld::class.java) {
                group = "chunk"
                description = "Convert the Anvil world ${world.name} to Polar"
                source.set(directory.resolve(world.source))
                chunks.set(world.chunks)
                classpath.from(converter, runtime)
                this.launcher.set(launcher)
                output.set(directory.resolve(".chunk/build/worlds/${app.id}/${world.name}.polar"))
            }
        project.rootProject.tasks.named("chunkArtifacts") { dependsOn(conversion) }
    }
}
