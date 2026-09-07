# chunk-proto (JVM)

Java protobuf and asynchronous gRPC bindings, usable from Kotlin and Java,
are generated from `proto/` by `:jvm:proto:generateProto`. Rust builds generate
matching bindings in `chunk-proto`. Generated sources are build outputs.

`Gameplay` implements configuration export and independent authenticated player
streams. Other services remain proposals until their implementations land.
Application clients will hide these internal provisioning and transport types.
