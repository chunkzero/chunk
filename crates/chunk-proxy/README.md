# Minecraft proxy

Implemented listener behavior and development commands. Run commands from the
repository root. Gameplay handoff and configuration-screen waiting during
backend wakeup are planned; the waiting world described below is current code.

```sh
cargo run -p chunk -- edge --bind 127.0.0.1:25565 --motd "My chunk edge"
```

Add `localhost:25565` to a Java Edition client's server list to see the MOTD
and ping. The proxy advertises 26.1 (protocol 775) and authenticates logins
through Mojang's session service in online mode. After configuration, players
enter a packet-simulated waiting world: an empty End void,
with no Minecraft server or JVM running. Players float at (8, 64, 8) in
spectator mode with movement speed set to zero. Movement packets are ignored.
Once loaded, the client sees “Preparing your server...” for ten seconds. Playable sessions and session
handoff are not available yet. Other versions receive a mismatch message;
legacy pre-1.7 pings and transfer handshakes are unsupported.

`mc-26-1` is the only version feature and is enabled by default. Features are
forwarded from `chunk` through the edge and proxy to `chunk-protocol`.
To select it explicitly:

```sh
cargo run -p chunk --no-default-features --features mc-26-1 -- edge
```

Without version features, protocol primitives remain available but the proxy
refuses to start. Features select releases; they do not translate versions.

`--max-connections` limits concurrent exchanges (default 1024). Each exchange
has a ten-second deadline, including authentication and Login Acknowledged.
Mojang requests use HTTPS with a five-second timeout and bounded responses;
failed verification never falls back to offline identities. The authenticated
UUID and profile properties come from Mojang, not the client's claimed UUID.
Compression defaults to 256 bytes; library callers can set
`Config::compression_threshold` to `None` to disable it. Ctrl-C or SIGTERM closes
the listener and active connections. Set `RUST_LOG=debug` to log individual
connection failures.

`Config::configuration_timeout` limits each configuration phase (default: five minutes).
The sixty-second total limbo cap overrides longer phase limits; shorter configured
limits still apply. Clients must send their
settings within ten seconds. Limbo derives its registries from the pinned
26.1 snapshot, with unused enchantments and dialogs omitted and dimension
timeline and client component tags included, then sends a 5×5 area of empty End chunks. The client
must acknowledge configuration, the chunk batch, and teleports. Loading and
teleport acknowledgments have fifteen-second deadlines.

Connections are evicted sixty seconds after login, including time spent in
configuration and loading; traffic does not reset this deadline. The proxy sends one
keepalive at a time, waits up to fifteen seconds for the matching response, and
sends the next ten seconds later. Writes have five-second deadlines; shutdown
cancels active connections. The limbo destination future returns the authenticated
transport and latest settings after outstanding acknowledgments are drained,
providing the transition point for later session handoff.

The listener encodes and compresses shared configuration, spawn, and title packets
once per protocol version at startup using its configured compression threshold.
Connections select these buffers by their negotiated protocol version; encryption remains specific to each connection.

To verify with a signed-in Java 26.1 client, join `localhost:25565`, confirm the
End sky renders with no terrain, confirm you float in place, and remain connected for
about a minute to verify automatic disconnection.

Packet generation and codec usage are covered in the
[protocol crate documentation](../chunk-protocol/src/lib.rs).
