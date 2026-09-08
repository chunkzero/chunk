plugins {
    java
}

val generateBackend =
    tasks.register<GenerateBackend>("generateBackend") {
        platformDirectory.set(rootProject.layout.projectDirectory)
        moduleName.set(project.path)
        outputDirectory.set(layout.buildDirectory.dir("generated/chunk"))
        inputs.dir(rootProject.file("packages/server/src"))
        inputs.files(rootProject.file("packages/compiler/bundle.mjs"), rootProject.file("pnpm-lock.yaml"))
        inputs.files(rootProject.fileTree("crates") { include("**/src/**", "**/Cargo.toml") })
        inputs.file(rootProject.file("Cargo.lock"))
    }
sourceSets.main {
    java.srcDir(generateBackend.flatMap { it.outputDirectory.dir("client/java") })
    java.srcDir(generateBackend.flatMap { it.outputDirectory.dir("client/java-client") })
}
tasks.named("compileJava") { dependsOn(generateBackend) }
plugins.withId("org.jetbrains.kotlin.jvm") {
    tasks.named("compileKotlin") { dependsOn(generateBackend) }
}
plugins.withId("application") {
    extensions.configure<DistributionContainer> {
        named("main") {
            contents {
                from(generateBackend.flatMap { it.outputDirectory.dir("backend") }) { into("backend") }
            }
        }
    }
}
