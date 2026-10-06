# chunk

A platform for building and running Minecraft servers as applications. You write gameplay as Java or Kotlin apps on
[Minestom](https://minestom.net) and backend logic in TypeScript: transactional queries and mutations over a built-in
database, reactive subscriptions, actions and scheduled jobs. chunk packages both into one release, starts gameplay
sessions on demand, and moves players between sessions without reconnecting them.

## Status

chunk is pre-release, and its APIs, storage and configuration still change without compatibility guarantees. It supports
Minecraft Java Edition 26.2. There is no published SDK release yet, so the way in is a checkout of this repository.
Local development works end to end, and [self-hosting](deploy/compose/README.md) runs on one Docker or Podman host,
which the CLI deploys to.

## How it fits together

Players connect to a gateway (`chunk-proxy`), which authenticates them, owns encryption and compression, and hands each
player to a session in a gameplay JVM over a native Minecraft connection. Each environment has one core: the backend
(`chunk-backend`), which runs the TypeScript functions on embedded V8 over SQLite, and control (`chunk-control`), which
places sessions and launches JVMs. Gateways, JVMs and the CLI reach core over the `chunk.sync.v1` gRPC protocol, whose
contracts live in `proto/`. `chunk dev` runs all of it on one machine. When self-hosted, the
[edge](crates/chunk-edge/README.md) accepts every player connection and routes it by hostname to an environment's
gateway, and the [management service](packages/management/README.md) stores releases, deploys them, and reconciles each
environment's machines through a provider. It can suspend idle environments, which the edge wakes when a player logs in.

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

It creates a Kotlin project (`--language java` for Java) and prints the commands to build and run it. Gameplay code
starts at [`jvm/multistom`](jvm/multistom/README.md); [`examples/arena`](examples/arena/README.md) shows the Java side
of the API. The [CLI README](crates/chunk-cli/README.md) covers every command: `dev` runs a project locally and rebuilds
it on change, `build` packages a release, `players` and `nodes` operate the local environment, and `auth login` and
`deploy` deploy to a self-hosted install. [`docs/distribution.md`](docs/distribution.md) covers packaging and publishing
the SDK.

## Self-hosting

[`deploy/compose`](deploy/compose/README.md) runs Postgres, management with its dashboard, and the edge on one Docker or
Podman host; management starts each environment's machines there. Its README covers setup and security, and deploying
with `chunk deploy`.

## Development

Toolchains are pinned in `mise.toml`, and [just](https://just.systems) runs the tasks; `just --list` shows them all. Run
the narrowest check for a change:

- `just fmt` formats everything and `just fmt-check` verifies it.
- `just lint` runs Clippy, Buf and oxlint.
- `just test` runs the Rust, JVM, SDK and dashboard tests. Management's tests run separately with Postgres; see its
  [README](packages/management/README.md).
- `just ready` runs most of CI's checks locally; run it before opening a pull request.

Design decisions are GitHub issues labelled `decision`. Editor settings for Zed, VS Code and IntelliJ are checked in
under `.zed/`, `.vscode/` and `.idea/`.

## Security

Please report vulnerabilities privately through GitHub's
[private vulnerability reporting](https://github.com/chunkzero/chunk/security/advisories/new), not in public issues.

## License

chunk is licensed under the [Functional Source License, Version 1.1, MIT Future License](LICENSE.md). You may use,
modify, and redistribute it for any purpose except offering it as a competing commercial product. Running your own
applications on chunk is not a competing use. Each release converts to the MIT license two years after it is published.
