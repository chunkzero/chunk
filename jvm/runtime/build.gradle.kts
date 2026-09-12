plugins { id("chunk.java-conventions") }
java { toolchain.languageVersion = JavaLanguageVersion.of(25) }
tasks.withType<JavaCompile>().configureEach { options.release = 25 }
dependencies {
    api(project(":jvm:backend-client"))
    implementation(project(":jvm:proto"))
    implementation(libs.grpc.netty)
    compileOnly(libs.jetbrains.annotations)
    testImplementation(libs.grpc.netty)
}
