# Minecraft proxy

Implemented listener behavior and development commands. Run commands from the repository root.

With `CHUNK_BACKEND_FILE=.chunk/backend.json` and `CHUNK_CONTROL_FILE=.chunk/control.json`, the proxy runs live backend
status/admission/routing hooks and waits in configuration while control provisions gameplay. Each delivery uses a
dedicated authenticated TCP path through the runtime to Minestom. Admission and preparation have a 45-second total
limit; arrival has a 20-second limit while packets continue flowing.

Same-proxy moves preserve authentication, encryption, compression and the public socket, including across JVMs. The
`proxy/move` hook approves the requested route after admission. The destination reserves capacity without creating a
player; source withdrawal must complete before native login to the destination. The proxy discards late source output,
acknowledges the PLAY-to-CONFIGURATION boundary, retains the latest settings, and relays the full destination
configuration. Preparation failure leaves the source playing. An unresolved cutover ends within a bounded deadline; it
does not replay packets or promise rollback.

```sh
chunk players --player <uuid> move --session-type arena --key arena
chunk players --player <uuid> drain --timeout-seconds 60
```

These local operator commands use the private control connection file. A move is queued for the owning proxy; a drain
retires the selected player's current runtime, queues replacement moves, and stops it when empty or at its persisted
deadline. The CLI reports completion only after confirmed runtime/JVM shutdown. Retain the printed operation ID with
`--operation` when retrying an uncertain command.

## Standalone service

The reusable proxy library is hosted by the `chunk-edge` binary:

```sh
CHUNK_BACKEND_FILE=.chunk/local/backend.json CHUNK_CONTROL_FILE=.chunk/local/control.json CHUNK_BIND=127.0.0.1:25565 cargo run -p chunk-edge
```

The edge requires backend/control discovery records. Library callers can omit `Config::platform` to use the
waiting-world fixture. The listener supports Java Edition 26.2 (protocol 776), with online authentication through
Mojang.

`mc-26-2` is enabled by default and forwarded from the edge to the proxy and protocol. Select it explicitly with
`cargo run -p chunk-edge --no-default-features --features mc-26-2`.

Without version features, protocol primitives remain available but the proxy refuses to start. Features select releases;
they do not translate versions.

`CHUNK_MAX_CONNECTIONS` limits concurrent exchanges (default 1024). Each exchange has a ten-second deadline, including
authentication and Login Acknowledged. Mojang requests use HTTPS with a five-second timeout and bounded responses;
failed verification never falls back to offline identities. The authenticated UUID and profile properties come from
Mojang, not the client's claimed UUID. Compression defaults to 256 bytes; library callers can set
`Config::compression_threshold` to `None` to disable it. Ctrl-C or SIGTERM closes the listener and active connections.
Set `RUST_LOG=debug` to log individual connection failures.

`Config::configuration_timeout` limits each configuration phase (default: five minutes). The sixty-second total limbo
cap overrides longer phase limits; shorter configured limits still apply. Clients must send their settings within ten
seconds. Limbo derives its registries from the pinned 26.2 snapshot, with unused enchantments and dialogs omitted and
dimension timeline and client component tags included, then sends a 5×5 area of empty End chunks. The client must
acknowledge configuration, the chunk batch, and teleports. Loading and teleport acknowledgments have fifteen-second
deadlines.

Connections are evicted sixty seconds after login, including time spent in configuration and loading; traffic does not
reset this deadline. The proxy sends one keepalive at a time, waits up to fifteen seconds for the matching response, and
sends the next ten seconds later. Writes have five-second deadlines; shutdown cancels active connections. The limbo
destination future returns the authenticated transport and latest settings after outstanding acknowledgments are
drained, providing the transition point for later session handoff.

The listener encodes and compresses shared configuration, spawn, and title packets once per protocol version at startup
using its configured compression threshold. Connections select these buffers by their negotiated protocol version;
encryption remains specific to each connection.

To verify with a signed-in Java 26.2 client, join `localhost:25565`, confirm the End sky renders with no terrain,
confirm you float in place, and remain connected for about a minute to verify automatic disconnection.

Packet generation and codec usage are covered in the [protocol crate documentation](../chunk-protocol/src/lib.rs).
