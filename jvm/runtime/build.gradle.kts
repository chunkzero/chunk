plugins {
    id("chunk.java-conventions")
    id("chunk.publishing-conventions")
}
java { toolchain.languageVersion = JavaLanguageVersion.of(25) }
tasks.withType<JavaCompile>().configureEach { options.release = 25 }
dependencies {
    implementation(project(":jvm:backend-client"))
    implementation(project(":jvm:proto"))
    implementation(libs.grpc.netty)
    compileOnly(libs.jetbrains.annotations)
    testImplementation(libs.grpc.netty)
}
