plugins { id("chunk.kotlin-conventions") }

dependencies {
    api(project(":jvm:backend-client"))
    api(libs.kotlinx.coroutines)
    testImplementation(libs.grpc.netty)
}
