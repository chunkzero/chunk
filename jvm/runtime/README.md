# Gameplay JVM

Minestom accepts prepared players through its normal loopback Minecraft listener.
The proxy owns public authentication, encryption and compression and relays
Minestom configuration and play. The Rust runtime supervises the JVM and relays
each player's connection. Minestom uses protocol 775, compatible with Java Edition 26.1.

Build `cargo build -p chunk` and `./gradlew :jvm:runtime:installDist`, then run
these in separate terminals from the repository root:

```sh
# Requires Java 25; Gradle resolves that toolchain for the gameplay module.
./target/debug/chunk runtime --java /path/to/java25/bin/java
./target/debug/chunk edge --runtime-file .chunk/runtime.json
```

The runtime generates separate child and proxy-facing credentials and writes the
private connection file only after authenticated registration and advancing ticks.
Diagnostics go to `.chunk/runtime.log`. Ctrl-C stops the supervised JVM within a
bounded deadline and removes the connection record. Check existing servers before
starting the proxy on port 25565. The default fixture supplies a grass session named `bridge`. Session commands can
create independent session instances and safely withdraw players before disposal.

Registration freezes deployment, runtime/process incarnation, machine profile,
artifact identity, protocol version and both JVM endpoints. Inventory RPCs
report ticks and prepared/attached/closed delivery bindings. A lifecycle outage
marks inventory unavailable and rejects new preparation, while existing TCP
streams remain independent. Repeated registration reconciles the same process;
it cannot change endpoints or configuration. A dead runtime loses its relays;
a dead JVM loses its worlds. Neither is recovered by replaying player bytes.

Preparation creates no player. Single-use capabilities expire after thirty seconds
and are exchanged through standard `chunk:delivery` login plugin packets. The
runtime replaces its upstream capability with the JVM capability on the second hop,
then forwards normal login success and relays bounded byte buffers. Minestom owns
configuration and player creation. Slow relay writes expire after five seconds;
login is bounded to five seconds, sockets to 128 and history to 4096 operations.
Native Minestom sockets handle buffering and graceful kicks.

Focused checks: `cargo test -p chunk-runtime` and `./gradlew :jvm:runtime:test`.
The standalone bridge remains available with `CHUNK_PROCESS_TOKEN` (at least 32
characters), `CHUNK_ENVIRONMENT` and `CHUNK_DEPLOYMENT`; without
`CHUNK_SUPERVISOR` it binds control on 25566 and uses a fixed fixture incarnation.
Production local launches should use the supervisor.

Sessions own their instances, event handlers and scoped resources. Session hooks
run through the process tick executor. Withdrawal waits for pending joins and
initialization, removes the player and runs its leave hook before releasing the
ownership fence. Arrival is reported after spawn and teleport acknowledgment.
