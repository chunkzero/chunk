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
as a competing commercial product. Running your own applications on chunk is
not a competing use. Each release converts to the MIT license two years after
it is published.
