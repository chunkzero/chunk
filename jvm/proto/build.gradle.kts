plugins {
    id("chunk.kotlin-conventions")
}

// Kotlin and Java gRPC bindings generated from the .proto files in /proto.
// The protobuf and grpc-kotlin plugins are added when the first service is
// wired; the module exists now so the layering is visible.
