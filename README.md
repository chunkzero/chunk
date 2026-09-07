# chunk

A Minecraft application platform: a sync engine with embedded JavaScript,
automatic gameplay-session provisioning, a Minestom server framework, and the
toolchain and control plane that connect them.

The v1 design has one authoritative backend per **environment** (`prod`, `beta`),
with multiple immutable **deployments** sharing its database. Sessions retain
their deployment's functions and assets while old deployments drain. Chunk
creates and places sessions automatically from server routing/demand policies; many
sessions can share one JVM. The initial embedded runtime uses `deno_core`/V8 with
language APIs and pure-JS packages, without Node compatibility.

Apps keep `app.toml` metadata, their own Gradle builds, and annotated JVM gameplay.
Server code owns optional matchmaking/queues. File-based `server/domains/` supplies
inherited proxy commands and `createHook` handlers; the connected JVM supplies
app-local commands. These APIs remain proposed, not implemented.

## Status

This branch implements the Rust Minecraft protocol and proxy listener, online
login authentication, configuration, and authenticated delivery to a Minestom bridge with prepared login admission.
Managed sessions, handoff, the sync engine, storage adapters, and the application
SDKs remain scaffolds. See [the bridge commands](jvm/runtime/README.md). The CLI currently runs `chunk edge`; planned roles and
commands are not implemented.

Dashboard/management scaffolding exists separately on
`feat/self-hosted-dashboard-assets` (at `5584cab` when this cleanup was prepared).
It is not included in this branch; asset publication remains proposed.

## Repository

| Path | Contents |
| --- | --- |
| `crates/` | Rust proxy, protocol, platform and toolchain crates |
| `jvm/` | Scaffolded chunk JVM framework, transport and build integration |
| `packages/server` | Scaffolded `@chunk/server` JavaScript package |
| `proto/` | Incomplete internal transport proposals |
| `docs/architecture.md` | Current repository boundaries and implementation gaps |

The intended platform design lives in the
[chunkzero knowledgebase](https://github.com/chunkzero/knowledgebase).
The [repository architecture](docs/architecture.md) maps that design to code.

## Development

Toolchains are pinned in `mise.toml`. Install [mise](https://mise.jdx.dev),
[just](https://just.systems), OpenSSL development headers and `pkg-config`, then
run `mise install`. Use `just --list` to find tasks and run the narrowest checks
for a change; `just ready` runs the full CI checks before a PR.

```sh
cargo run -p chunk -- edge --bind 127.0.0.1:25565 --motd "My chunk server"
```

The proxy supports Java Edition 26.1 (protocol 775). See the
[proxy documentation](crates/chunk-proxy/README.md) for current waiting-world
behavior, timeouts, feature selection and manual verification.

## License

chunk is licensed under the
[Functional Source License, Version 1.1, MIT Future License](LICENSE.md).
You may use, modify, and redistribute it for any purpose except offering it
as a competing commercial product. Running your own applications on chunk is
not a competing use. Each release converts to the MIT license two years after
it is published.
