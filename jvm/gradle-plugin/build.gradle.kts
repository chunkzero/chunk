import org.jetbrains.dokka.gradle.tasks.DokkaGeneratePublicationTask

plugins {
    alias(libs.plugins.kotlin.jvm)
    alias(libs.plugins.kotlin.sam.receiver)
    alias(libs.plugins.dokka)
    `java-gradle-plugin`
    `maven-publish`
}

group = "dev.chunkzero"
version = libs.versions.chunk.get()

kotlin {
    jvmToolchain(21)
    compilerOptions { allWarningsAsErrors = true }
}

samWithReceiver { annotation("org.gradle.api.HasImplicitReceiver") }

dependencies {
    implementation(libs.gson)
    implementation(libs.shadow.gradle.plugin)
    implementation(libs.asm)
    compileOnly(libs.kotlin.gradle.plugin)
    testImplementation(gradleTestKit())
    testImplementation(platform(libs.junit.bom))
    testImplementation(libs.junit.jupiter)
    testRuntimeOnly(libs.junit.platform.launcher)
}

gradlePlugin {
    plugins {
        create("chunkSettings") {
            id = "dev.chunkzero.chunk.settings"
            implementationClass = "dev.chunkzero.gradle.ChunkSettingsPlugin"
        }
        create("chunk") {
            id = "dev.chunkzero.chunk"
            implementationClass = "dev.chunkzero.gradle.ChunkPlugin"
        }
        create("chunkKotlin") {
            id = "dev.chunkzero.chunk.kotlin"
            implementationClass = "dev.chunkzero.gradle.ChunkKotlinPlugin"
        }
    }
}

tasks.register<Jar>("javadocJar") {
    archiveClassifier.set("javadoc")
    from(tasks.named<DokkaGeneratePublicationTask>("dokkaGeneratePublicationHtml").flatMap { it.outputDirectory })
}

java {
    withSourcesJar()
    withJavadocJar()
}

tasks.withType<AbstractArchiveTask>().configureEach {
    isPreserveFileTimestamps = false
    isReproducibleFileOrder = true
}

tasks.jar { manifest.attributes("Implementation-Version" to project.version) }
publishing {
    repositories {
        maven {
            name = "sdk"
            url = uri(providers.gradleProperty("chunk.sdkRepository").orElse("../../build/sdk/maven"))
        }
        maven {
            name = "functionalTest"
            url = uri(layout.buildDirectory.dir("functional-test-repository"))
        }
    }
}

val fixtureJava = javaToolchains.launcherFor { languageVersion = JavaLanguageVersion.of(25) }
tasks.test {
    dependsOn("publishAllPublicationsToFunctionalTestRepository")
    useJUnitPlatform()
    systemProperty(
        "chunk.test.plugin.repository",
        layout.buildDirectory
            .dir("functional-test-repository")
            .get()
            .asFile
            .toURI(),
    )
    systemProperty(
        "chunk.test.java.home",
        fixtureJava
            .get()
            .metadata.installationPath.asFile.absolutePath,
    )
    systemProperty("chunk.plugin.version", project.version)
    systemProperty("chunk.kotlin.version", libs.versions.kotlin.get())
    systemProperty(
        "chunk.test.repository",
        layout.projectDirectory
            .dir("../..")
            .asFile.absolutePath,
    )
}
