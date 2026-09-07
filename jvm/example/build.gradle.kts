plugins {
    id("chunk.kotlin-conventions")
    application
}

kotlin {
    jvmToolchain(25)
    compilerOptions { jvmTarget = org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_25 }
}

dependencies { implementation(project(":jvm:runtime")) }

application { mainClass = "dev.chunkzero.runtime.BridgeMainKt" }

val localJava = javaToolchains.launcherFor { languageVersion = JavaLanguageVersion.of(25) }

abstract class WriteJavaExecutable : DefaultTask() {
    @get:Input
    abstract val executable: Property<String>

    @get:OutputFile
    abstract val outputFile: RegularFileProperty

    @TaskAction
    fun write() {
        outputFile.get().asFile.writeText(executable.get())
    }
}

tasks.register<WriteJavaExecutable>("writeJavaExecutable") {
    executable.set(localJava.map { it.executablePath.asFile.absolutePath })
    outputFile.set(layout.buildDirectory.file("java-executable.txt"))
}
