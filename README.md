# chunk

A platform for building and running Minecraft servers as applications. You write gameplay as Java or Kotlin apps on
[Minestom](https://minestom.net) and backend logic in TypeScript: transactional queries and mutations over a built-in
database, reactive subscriptions, actions and scheduled jobs. chunk packages both into one release, starts gameplay
sessions on demand, and moves players between sessions without reconnecting them.

## Status

chunk is pre-release, and its APIs, storage and configuration still change without compatibility guarantees. It supports
Minecraft Java Edition 26.2. There is no published SDK release yet, so the way in is a checkout of this repository.
Local development works end to end, and [self-hosting](deploy/compose/README.md) runs on one Docker or Podman host. The
CLI does not deploy yet; releases are deployed through the management API.

## How it fits together

Players connect to a gateway (`chunk-proxy`), which authenticates them, owns encryption and compression, and hands each
player to a session in a gameplay JVM over a native Minecraft connection. Each environment has one core: the backend
(`chunk-backend`), which runs the TypeScript functions on embedded V8 over SQLite, and control (`chunk-control`), which
places sessions and launches JVMs. Gateways, JVMs and the CLI reach core over the `chunk.sync.v1` gRPC protocol.
`chunk dev` runs all of it on one machine. When self-hosted, the edge (`chunk-edge`) accepts every player connection and
routes it by hostname to an environment's gateway, and the management service (`packages/management`) stores releases,
deploys them, and reconciles each environment's machines through a provider. It can suspend idle environments, which the
edge wakes when a player logs in.

## Repository

| Path                                                                | Contents                                                                                   |
| ------------------------------------------------------------------- | ------------------------------------------------------------------------------------------ |
| `crates/chunk-cli`, `crates/chunk-build`                            | The `chunk` CLI, compiler and release packaging, and the TypeScript SDK                    |
| `crates/chunk-environment`                                          | The environment process, running core and the gateway                                      |
| `crates/chunk-backend`, `chunk-store`, `chunk-js`, `chunk-contract` | The backend: sync engine, storage, embedded JavaScript and contracts                       |
| `crates/chunk-control`                                              | Session placement, capacity and JVM supervision                                            |
| `crates/chunk-proxy`, `chunk-protocol`, `chunk-protocol-*`          | The gateway's Minecraft proxy and protocol codecs                                          |
| `crates/chunk-edge`                                                 | The edge in front of self-hosted environments                                              |
| `crates/chunk-jvm`                                                  | The runner on JVM machines                                                                 |
| `crates/chunk-management`                                           | A Rust client for the management API                                                       |
| `crates/chunk-proto`, `chunk-service`, `chunk-bench`                | Generated gRPC bindings, service helpers and workload benchmarks                           |
| `jvm/`                                                              | Java runtime and Minestom adapter, backend clients, Kotlin adapters, and the Gradle plugin |
| `packages/management`                                               | The management service (TypeScript on Bun and Postgres)                                    |
| `packages/dashboard`                                                | The dashboard management serves                                                            |
| `proto/`                                                            | The `chunk.sync.v1` and `chunk.management.v1` contracts                                    |
| `deploy/compose/`                                                   | The self-hosting bundle                                                                    |
| `examples/local/`, `examples/java/`                                 | A Kotlin example project and a Java one                                                    |
| `scripts/`                                                          | SDK packaging, Maven publishing and end-to-end smoke tests                                 |
| `docs/distribution.md`                                              | Packaging, installing and publishing the SDK                                               |

Design decisions are GitHub issues labelled `decision`.

## Getting started

Install [mise](https://mise.jdx.dev), OpenSSL development headers and `pkg-config`, then, from the repository root:

```sh
mise install
just local
```

This builds the [local example](examples/local/README.md) and runs it. Join `localhost:25565` with a signed-in Minecraft
Java Edition 26.2 client; Ctrl-C stops everything.

To start your own project from this checkout:

```sh
just toolchain
target/debug/chunk create ../my-server --chunk-source .
```

It creates a Kotlin project (`--language java` for Java) and prints the commands to build and run it. `chunk --help`
lists the CLI's commands: `dev` runs a project locally and rebuilds it on change, `build` packages a release, and
`players` and `nodes` operate the local environment.

## Self-hosting

[`deploy/compose`](deploy/compose/README.md) runs Postgres, management with its dashboard, and the edge on one Docker or
Podman host; management starts each environment's machines there. Its README covers setup, security and deploying a
release.

## Development

Toolchains are pinned in `mise.toml`, and [just](https://just.systems) runs the tasks; `just --list` shows them all. Run
the narrowest check for a change:

- `just fmt` formats everything and `just fmt-check` verifies it.
- `just lint` runs Clippy, Buf and oxlint.
- `just test` runs the Rust, JVM, SDK and dashboard tests. Management's tests run separately with Postgres; see its
  [README](packages/management/README.md).
- `just ready` runs most of CI's checks locally; run it before opening a pull request.

Editor settings for Zed, VS Code and IntelliJ are checked in under `.zed/`, `.vscode/` and `.idea/`.

## Security

Please report vulnerabilities privately through GitHub's
[private vulnerability reporting](https://github.com/chunkzero/chunk/security/advisories/new), not in public issues.

## License

chunk is licensed under the [Functional Source License, Version 1.1, MIT Future License](LICENSE.md). You may use,
modify, and redistribute it for any purpose except offering it as a competing commercial product. Running your own
applications on chunk is not a competing use. Each release converts to the MIT license two years after it is published.
