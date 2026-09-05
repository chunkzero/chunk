# chunk-runtime (JVM)

The session-process implementation of chunk's JVM SDK. This module will
connect outbound to the Rust supervisor, execute session commands, integrate
player frame streams with Minestom, and report readiness and session state.
Generated function clients use its internal transport implementation.

This remains a build scaffold. Generated protobuf types are an implementation
dependency. A public API module will be introduced with the first
developer-facing SDK types.
