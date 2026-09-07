# chunk-runtime (JVM)

The Minestom bridge accepts prepared players through its normal loopback Minecraft
listener. The Rust proxy owns online authentication, encryption and compression;
Minestom owns login admission, configuration and play. A standard `chunk:delivery`
login plugin exchange presents the capability issued by the authenticated control
RPC. The pinned Minestom release uses protocol 775, compatible with Java Edition 26.1.

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

Control RPCs require the process credential. Preparation validates deployment,
process generation, protocol and registry digest and issues a single-use,
30-second capability. Minestom rejects missing, expired, mismatched or replayed
capabilities before creating a player. The prepared identity includes profile
properties; client settings travel through normal configuration packets.
Owner generations fence deliveries. Terminal operation records release their live
connection references, and history is bounded to 4096 deliveries per fixture.
Minestom handles socket buffering and graceful kicks. Proxy writes and the login
plugin exchange have five-second deadlines; configuration uses the proxy's
configured deadline.

Focused verification: `./gradlew :jvm:runtime:test` and `cargo test -p chunk-proxy`.

Session management, backend clients, supervisor registration and control-plane
ownership follow in subsequent changes. The standalone bridge's process
generation is fixed for this initial integration fixture; restart its proxy
alongside it. The local transport uses loopback and a shared process credential.
