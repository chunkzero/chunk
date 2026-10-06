plugins {
    id("chunk.java-conventions")
    id("chunk.publishing-conventions")
}
java { toolchain.languageVersion = JavaLanguageVersion.of(25) }
tasks.withType<JavaCompile>().configureEach { options.release = 25 }

// The Gradle plugin runs the converter on an app's runtime classpath, which supplies the app's
// Minestom: upstream or multistom. It compiles against upstream and is tested on both.
dependencies {
    compileOnly(libs.minestom)
    compileOnly(libs.jetbrains.annotations)
    implementation(libs.polar)
    testImplementation(libs.minestom)
}

val multistomTestRuntime =
    configurations.resolvable("multistomTestRuntimeClasspath") {
        extendsFrom(configurations.testImplementation.get(), configurations.testRuntimeOnly.get())
        exclude(group = "net.minestom", module = "minestom")
        attributes {
            attribute(Usage.USAGE_ATTRIBUTE, objects.named(Usage.JAVA_RUNTIME))
            attribute(Category.CATEGORY_ATTRIBUTE, objects.named(Category.LIBRARY))
            attribute(LibraryElements.LIBRARY_ELEMENTS_ATTRIBUTE, objects.named(LibraryElements.JAR))
            attribute(Bundling.BUNDLING_ATTRIBUTE, objects.named(Bundling.EXTERNAL))
        }
    }
dependencies { "multistomTestRuntimeClasspath"(libs.multistom) }

val multistomTest =
    tasks.register<Test>("multistomTest") {
        description = "Runs the tests against multistom."
        group = "verification"
        testClassesDirs =
            sourceSets.test
                .get()
                .output.classesDirs
        classpath = sourceSets.test.get().output + sourceSets.main.get().output + multistomTestRuntime.get()
    }
tasks.check { dependsOn(multistomTest) }
tasks.withType<Test>().configureEach { jvmArgs("--enable-native-access=ALL-UNNAMED") }
