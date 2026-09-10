# chunk-proto (JVM)

Java protobuf and asynchronous gRPC bindings, usable from Kotlin and Java,
are generated from `proto/` by `:jvm:proto:generateProto`. Rust builds generate
matching bindings in `chunk-proto`. Generated sources are build outputs.

`Backend`, `LocalControl`, `Gameplay`, `Supervisor`, `ProcessControl` and `Players`
have implementations for backend calls/watches, placement, process/session
lifecycle and player operations. `Gameplay` prepares and withdraws deliveries;
player configuration and play travel over native Minecraft TCP connections.
`Directory`, `Runtime` and `EdgeCall` remain proposals.

These are internal platform contracts. Application code uses the Java/Kotlin
session and generated backend APIs, which retain deployment and caller identity.
