# chunk-runtime (JVM)

The session-process implementation of chunk's JVM SDK. This module will
connect outbound to the Rust supervisor, execute session commands, integrate
player frame streams with Minestom, and report readiness and session state.
Generated function clients use its internal transport implementation.

This remains a build scaffold. Public developer-facing types belong in
`jvm/api`; generated protobuf types are an implementation dependency.
There is no separate block framework or required block runtime plugin.
