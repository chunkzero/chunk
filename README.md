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

The local implementation includes online authentication, live JavaScript
admission/routing, automatic session placement, supervised Minestom JVMs,
SQLite transactions and reactive subscriptions. Players move between sessions
and JVMs on the same public connection. The backend and control authority can
restart independently of surviving gameplay streams.

Run the [local example](examples/local/README.md) with `just local`. It packages
an immutable deployment, starts all services, and demonstrates persistent coins,
subscriptions, session moves and drain. Ctrl-C stops its services and gameplay JVMs.
The broader app/domain SDK, hosted adapters, cross-proxy transfers, world persistence
and overlapping deployment rollouts remain deferred.

Dashboard/management scaffolding exists separately on
`feat/self-hosted-dashboard-assets` (at `5584cab` when this cleanup was prepared).
It is not included in this branch; asset publication remains proposed.

## Repository

| Path | Contents |
| --- | --- |
| `crates/` | Rust proxy, protocol, platform and toolchain crates |
| `jvm/` | Managed Minestom sessions, backend client, generated transport and example |
| `packages/server` | Typed `@chunk/server` declarations and document API |
| `proto/` | Generated lifecycle/backend/control contracts and remaining transport proposals |
| `examples/local/` | TypeScript backend source and local project configuration |
| `docs/architecture.md` | Broader platform design proposals |

The intended platform design lives in the
[chunkzero knowledgebase](https://github.com/chunkzero/knowledgebase).
The [repository architecture](docs/architecture.md) records the broader proposed design.

## Development

Toolchains are pinned in `mise.toml`. Install [mise](https://mise.jdx.dev),
[just](https://just.systems), OpenSSL development headers and `pkg-config`, then
run `mise install`. Use `just --list` to find tasks and run the narrowest checks
for a change; `just ready` runs the full CI checks before a PR.

```sh
just local
```

The proxy supports Java Edition 26.1 (protocol 775). See the
[proxy documentation](crates/chunk-proxy/README.md) for managed delivery, timeouts,
feature selection and the standalone waiting-world fixture.

## License

chunk is licensed under the
[Functional Source License, Version 1.1, MIT Future License](LICENSE.md).
You may use, modify, and redistribute it for any purpose except offering it
as a competing commercial product. Running your own applications on chunk is
not a competing use. Each release converts to the MIT license two years after
it is published.
