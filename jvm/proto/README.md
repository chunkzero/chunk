# chunk-proto (JVM)

Java protobuf and asynchronous gRPC bindings, usable from Kotlin and Java, are generated from `proto/` by
`:jvm:proto:generateProto`. Rust builds generate matching bindings in `chunk-proto`. Generated sources are build
outputs.

Each JVM reaches core over the `chunk.sync.v1` `Core` service, which carries its backend calls and watches,
process/session lifecycle and player deliveries; player configuration and play travel over native Minecraft TCP
connections.

These are internal platform contracts. Application code uses the Java/Kotlin session and generated backend APIs, which
retain deployment and caller identity.
