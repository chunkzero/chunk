plugins {
    id("chunk.kotlin-conventions")
}

dependencies {
    api(project(":jvm:api"))
    implementation(project(":jvm:proto"))
}
