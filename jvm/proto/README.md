# Protocol bindings (JVM)

Java protobuf messages and gRPC stubs for the contracts in [`proto/`](../../proto): `chunk.sync.v1`, which gameplay
JVMs, gateways and the CLI speak to core, and `chunk.management.v1`. `./gradlew :jvm:proto:generateProto` generates
them; the sources are build outputs. Rust code uses the matching bindings in `crates/chunk-proto`.

A gameplay JVM reaches core only over the `chunk.sync.v1` `Core` service, for its backend calls and watches, its
registration and reports, and the sessions, player deliveries and session methods core assigns it. Player traffic goes
from the gateway straight to the JVM's Minecraft listener.

These are internal contracts. Gameplay code uses the [runtime](../runtime/README.md) and the generated
[backend client](../backend-client/README.md) instead.
