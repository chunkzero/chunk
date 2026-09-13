import com.google.protobuf.gradle.id

plugins {
    id("chunk.java-conventions")
    id("chunk.publishing-conventions")
    alias(libs.plugins.protobuf)
}

dependencies {
    api(libs.protobuf.java)
    api(libs.grpc.protobuf)
    api(libs.grpc.stub)
}

sourceSets.main {
    proto.srcDir(rootProject.file("proto"))
}

dokka {
    dokkaSourceSets.configureEach { suppressGeneratedFiles.set(false) }
}

protobuf {
    protoc { artifact = "com.google.protobuf:protoc:${libs.versions.protobuf.get()}" }
    plugins {
        id("grpc") { artifact = "io.grpc:protoc-gen-grpc-java:${libs.versions.grpc.get()}" }
    }
    generateProtoTasks {
        all().configureEach { plugins { id("grpc") } }
    }
}
