# chunk-runtime (JVM)

The initial Minestom bridge accepts authenticated players over gRPC. It never
opens a Minecraft listener. The Rust proxy owns online authentication, encryption,
compression and configuration; the bridge supplies its full registry data and
uses a custom `PlayerConnection` for plain play packets. The pinned Minestom
release uses protocol 775, compatible with Java Edition 26.1.

Build with `cargo build -p chunk` and `./gradlew :jvm:runtime:installDist`.
The JVM application needs Java 25; Gradle resolves its pinned major toolchain.
Run these commands from the repository root, after checking that ports 25565
and 25566 are free:

```sh
# Share this environment with both processes; never commit the token.
export CHUNK_PROCESS_TOKEN="$(openssl rand -hex 32)"
export CHUNK_ENVIRONMENT=local
export CHUNK_DEPLOYMENT=local
# JAVA_HOME must point to Java 25 when running the installed launcher.
jvm/runtime/build/install/runtime/bin/runtime &
bridge_pid=$!
trap 'kill "$bridge_pid"; wait "$bridge_pid"' EXIT
./target/debug/chunk edge --gameplay http://127.0.0.1:25566
```

Join `127.0.0.1:25565` with an authenticated official 26.1 client. The bridge
currently provides one grass world named `bridge`. Without `--gameplay`, the
proxy retains its bounded waiting-world behavior.

Both RPCs require the process credential. Delivery validates deployment,
process generation, protocol and registry digest before creating a player.
Owner generations reject duplicate deliveries and stale cleanup; frame streams
are never replayed. Output is limited to 256 packets and 8 MiB per player;
slow peers are disconnected. Proxy writes and gRPC queue waits have five-second
deadlines. Configuration has the proxy's configured deadline. Empty streams
have ten seconds to present a delivery.

Focused verification: `./gradlew :jvm:runtime:test` and `cargo test -p chunk-proxy`.
The official client has been used to verify fresh-build login, registry exchange,
world loading, movement and sustained connection through the bridge.

Session management, backend clients, supervisor registration and control-plane
ownership follow in subsequent changes. The standalone bridge's process
generation is fixed for this initial integration fixture; restart its proxy
alongside it. The local transport uses loopback and a shared process credential.
