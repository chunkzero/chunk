# chunk JVM API

The public Java/Kotlin SDK module, absorbing the essential API previously
planned as block: session lifecycle, player references, scoped cleanup,
asset access, and typed function-client support. Minestom remains visible.

This module is a build scaffold; it does not implement sessions yet. Transport
messages and process wiring belong in `jvm/runtime`, not in the public API.
Optional overworld features build on this SDK.
