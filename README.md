# chunk

A portable Minecraft server runtime.

chunk wraps a Minecraft server in a small supervisor that owns the server's
lifecycle and its network edge. The same runtime runs wherever you want to
host: a cloud sandbox, a container, a bare VM, or a plain Java process on your
laptop. Hosting is a backend detail; the server and the players see the same
thing everywhere.

## How it fits together

Much like `cloudflared` sits between an origin and the edge, chunk sits between
the Minecraft server and the players:

- **Runtime (Rust, `crates/`)** runs next to the server process. It starts,
  stops, pauses, and health-checks the server, and exposes the server's
  connection over a tunnel so the host does not need to expose raw TCP.
- **Connector (Rust, `crates/`)** runs wherever players connect. It listens on
  a normal Minecraft port, finds or wakes the right server, and tunnels each
  player's session to its runtime.
- **Server-side integration (Kotlin, `jvm/`)** runs inside the JVM alongside
  the Minecraft server, handling anything that has to happen in-process such
  as coordinated saves before a pause.

Hosts plug in behind the runtime and connector. The first targets are a
cloud sandbox backend, a container runtime, and a bare local Java process.

## Development

Toolchains are pinned in `mise.toml`. Install [mise](https://mise.jdx.dev)
and [just](https://just.systems), then:

```sh
mise install
just ready
```

`just --list` shows the available tasks.

## License

chunk is licensed under the
[Functional Source License, Version 1.1, MIT Future License](LICENSE.md).
You may use, modify, and redistribute it for any purpose except offering it
as a competing commercial product. Each release converts to the MIT license
two years after it is published.
