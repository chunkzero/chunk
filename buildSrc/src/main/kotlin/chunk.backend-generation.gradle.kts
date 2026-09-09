plugins {
    java
}

val generateBackend =
    tasks.register<GenerateBackend>("generateBackend") {
        platformDirectory.set(rootProject.layout.projectDirectory)
        moduleName.set(project.path)
        outputDirectory.set(layout.buildDirectory.dir("generated/chunk"))
        inputs.dir(rootProject.file("packages/server/src")).withPathSensitivity(PathSensitivity.RELATIVE)
        inputs
            .files(
                listOf(
                    "packages/compiler/bundle.mjs",
                    "packages/compiler/package.json",
                    "packages/server/package.json",
                    "package.json",
                    "pnpm-lock.yaml",
                    "pnpm-workspace.yaml",
                    "Cargo.toml",
                    "Cargo.lock",
                    "mise.toml",
                ).map { rootProject.file(it) },
                rootProject.fileTree("crates") { include("**/src/**", "**/Cargo.toml", "**/build.rs") },
            ).withPathSensitivity(PathSensitivity.RELATIVE)
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
