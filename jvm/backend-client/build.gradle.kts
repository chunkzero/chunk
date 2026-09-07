plugins { id("chunk.kotlin-conventions") }

dependencies {
    api(project(":jvm:proto"))
    testImplementation(libs.grpc.netty)
}
