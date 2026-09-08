plugins { id("chunk.kotlin-conventions") }

dependencies {
    api(project(":jvm:proto"))
    api(project(":jvm:backend-java"))
    api(libs.kotlinx.coroutines)
    testImplementation(libs.grpc.netty)
}
