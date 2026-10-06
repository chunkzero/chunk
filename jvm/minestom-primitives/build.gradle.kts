plugins {
    id("chunk.java-conventions")
    id("chunk.publishing-conventions")
}
java { toolchain.languageVersion = JavaLanguageVersion.of(25) }
tasks.withType<JavaCompile>().configureEach { options.release = 25 }
dependencies {
    api(project(":jvm:runtime"))
    // Apps bring their own Minestom; this module only uses API upstream Minestom shares.
    compileOnly(libs.upstream.minestom)
    compileOnly(libs.jetbrains.annotations)
    testImplementation(libs.upstream.minestom)
    testImplementation(project(":jvm:proto"))
}
