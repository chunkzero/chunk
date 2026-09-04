# chunk

**Cloudflare for Minecraft servers.** chunk is the toolchain, the edge, the
session runtime, and the control plane for the chunkzero platform. It owns the
player's connection, runs an application's coordination code at the edge, and
supervises the JVM processes where play happens.

An application has two tiers, chosen by what the code needs:

- **Edge** code is TypeScript shaped like Convex: a schema-defined database,
  `query`, `mutation` and `action` functions, reactive queries, a durable
  scheduler, and Minecraft primitives such as the login decision, queues,
  players and packs. chunk compiles it into one module and runs it in a
  per-app QuickJS runtime with no ambient capabilities.
- **Session** code is Kotlin or Java with Minestom fully visible, written
  against the [block](https://github.com/chunkzero) framework and run in JVM
  processes that chunk starts, supervises and restarts with players held.

chunk sits between them the way a proxy sits between players and backends,
except a backend is one session with a lifecycle rather than a whole server,
and one process hosts many. Players stay connected to the edge across moves
and restarts; no app code ever opens a socket.

## Repository

| path            | what                                                             |
| --------------- | ---------------------------------------------------------------- |
| `crates/`       | Rust: the `chunk` binary and the platform and toolchain crates   |
| `jvm/`          | Kotlin: the session-side runtime client, build API, Gradle plugin |
| `packages/`     | TypeScript: `@chunk/edge`, the module edge code imports          |
| `proto/`        | the internal gRPC transport, shared by Rust and the JVM          |
| `docs/`         | architecture notes for this repository                           |

[docs/architecture.md](docs/architecture.md) maps processes to crates and
records the boundaries between them. The platform design itself, including
the edge API, the session framework and the decisions behind the two tiers,
lives in the [chunkzero knowledgebase](https://github.com/chunkzero/knowledgebase).

## Development

Toolchains are pinned in `mise.toml`. Install [mise](https://mise.jdx.dev)
and [just](https://just.systems), plus OpenSSL development headers and
`pkg-config` for the proxy's RSA/AES implementation, then:

```sh
mise install
just ready
```

`just --list` shows the available tasks.

### Run the proxy

```sh
cargo run -p chunk -- edge --bind 127.0.0.1:25565 --motd "My chunk edge"
```

Add `localhost:25565` to a Java Edition client's server list to see the MOTD
and ping. The proxy advertises 26.1 (protocol 775) and authenticates logins
through Mojang's session service in online mode. A successful Login Acknowledged
produces an `authenticated player reached configuration` log entry, then closes
the connection until configuration parking is implemented. Playable sessions
are not available yet. Other versions receive a mismatch message; legacy
pre-1.7 pings and transfer handshakes are unsupported.

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

Packet generation and codec usage are covered in the
[protocol crate documentation](crates/chunk-protocol/src/lib.rs).

## License

chunk is licensed under the
[Functional Source License, Version 1.1, MIT Future License](LICENSE.md).
You may use, modify, and redistribute it for any purpose except offering it
as a competing commercial product. Running your own applications on chunk is
not a competing use. Each release converts to the MIT license two years after
it is published.
