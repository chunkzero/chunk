# chunk

A Minecraft server runtime for [AWS Lambda MicroVMs](https://aws.amazon.com/lambda/lambda-microvms/).

Lambda MicroVMs are Firecracker-backed, single-tenant VMs that start from a
pre-initialized memory and disk snapshot, suspend when idle, and resume on
demand. chunk packages a Minecraft server into that model: a world boots in
milliseconds from a snapshot, pauses when nobody is online, and costs nothing
while suspended.

## How it fits together

MicroVMs only accept inbound traffic over HTTPS, WebSocket, or gRPC on an
authenticated endpoint. Minecraft speaks raw TCP. chunk bridges the two the
way `cloudflared` bridges an origin to Cloudflare's edge:

- **Runtime (Rust, `crates/`)** runs inside the MicroVM. It supervises the
  server process, answers the Lambda lifecycle hooks (`/ready`, `/validate`,
  `/run`, `/suspend`, `/terminate`), and exposes the server's TCP port over
  the MicroVM's WebSocket endpoint.
- **Connector (Rust, `crates/`)** runs wherever players connect. It listens on
  a normal Minecraft port, launches or resumes the right MicroVM, and tunnels
  each player's TCP session to the runtime.
- **Server-side integration (Kotlin, `jvm/`)** runs inside the JVM alongside
  the Minecraft server, handling snapshot-safe startup, save-on-suspend, and
  anything that needs to happen in-process.

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
