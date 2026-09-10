# chunk

A Minecraft application platform: a sync engine with embedded JavaScript,
automatic gameplay-session provisioning, a Minestom server framework, and the
toolchain and control plane that connect them.

Each **environment** (`prod`, `beta`) has one authoritative backend. Multiple
immutable **deployments** can share its database, with session clients bound to
their deployment's functions. Chunk
creates and places sessions automatically from server routing/demand policies; many
sessions can share one JVM. The initial embedded runtime uses `deno_core`/V8 with
language APIs and pure-JS packages, without Node compatibility.

Apps keep `app.toml` metadata, their own Gradle builds, and Java or Kotlin gameplay.
Each app JAR registers one `SessionProvider` that creates fresh session state.
The build generates typed backend clients and packages all apps, shared
dependencies, backend code and assets into one portable release.

## Status

The local implementation includes online authentication, live JavaScript
admission/routing, automatic session placement, supervised Minestom JVMs,
SQLite transactions and reactive subscriptions. Players move between sessions
and JVMs on the same public connection. Standalone backend and control services can restart independently. The development
runner embeds services and stops the stack if one fails.

Run the [local example](examples/local/README.md) with `just local`. It packages
an immutable deployment, starts all services, and demonstrates persistent coins,
subscriptions, session moves and drain. Ctrl-C stops its services and gameplay JVMs.
Domains, annotation registration and dependency injection, the broader event API,
hosted adapters, cross-proxy transfers, world persistence and overlapping
deployment rollouts remain deferred. Queues and matchmaking remain server-owned policy.

Dashboard/management scaffolding exists separately on
`feat/self-hosted-dashboard-assets`. It is not included in this branch;
dashboard integration and asset uploads remain deferred.

## Repository

| Path | Contents |
| --- | --- |
| `crates/` | Rust proxy, protocol, platform and toolchain crates |
| `jvm/` | Java sessions and backend clients, optional Kotlin adapters, transport and Gradle plugin |
| `packages/server` | Typed `@chunk/server` declarations and document API |
| `proto/` | Generated lifecycle/backend/control contracts and remaining transport proposals |
| `examples/local/` | App modules, shared gameplay, TypeScript backend and project configuration |
| `examples/java/` | Java consumer using the runtime and generated typed backend API |
| `docs/architecture.md` | Implemented boundaries and deferred platform design |

The intended platform design lives in the
[chunkzero knowledgebase](https://github.com/chunkzero/knowledgebase).
The [repository architecture](docs/architecture.md) maps the implementation and
identifies the remaining proposals.

## Development

The [Gradle plugin](jvm/gradle-plugin/README.md) discovers app projects from Rust
metadata and compiles shared Java bindings, with an explicit Kotlin facade opt in.

Toolchains are pinned in `mise.toml`. Install [mise](https://mise.jdx.dev),
[just](https://just.systems), OpenSSL development headers and `pkg-config`, then
run `mise install`. Use `just --list` to find tasks and run the narrowest checks
for a change; `just ready` runs the full CI checks before a PR.

Root Gradle `test` and `assemble` tasks cover the framework modules and plugin.
The local example is a separate Gradle build. `just consumers` builds the real
[Java consumer](examples/java/README.md) and Kotlin example from scratch source
copies using the prepared CLI, then checks their release archives and runtime
classpaths. It starts no gameplay or backend services and also runs in CI and
`just ready`.

```sh
just local
```

The proxy supports Java Edition 26.1 (protocol 775). See the
[proxy documentation](crates/chunk-proxy/README.md) for managed delivery, timeouts,
feature selection and standalone hosting.

## CLI and services

`chunk` is the developer CLI (`crates/chunk-cli`):

- `chunk inspect PROJECT` reads project and app metadata as JSON without building.
- `chunk gen PROJECT --target java|kotlin|typescript` compiles backend code and generates selected clients.
- `chunk build PROJECT` runs the project Gradle wrapper and packages backend code,
  app JARs, dependencies and assets as `PROJECT/dist/<id>.tar.gz` and `PROJECT/dist/<id>/`.
- `chunk dev PROJECT` (`chunk local`) builds that release and runs the development
  stack with embedded services and child JVMs. It uses the Gradle-selected Java
  executable; `--java PATH` can override it.
- `chunk players` operates on local players.
- `chunk auth login` prompts for Chunk Cloud or a custom platform URL; use
  `--cloud` or `--url URL` for non-interactive selection. `chunk login` is an alias.
- `chunk auth status` shows the effective target and authentication implementation status.
- `chunk deploy [PROJECT]`, `chunk upload ARTIFACT`, `chunk logs [--follow]`,
  `chunk deployments list`, `chunk environments list`, and `chunk apps list`
  are explicit, non-successful stubs. Deploy, logs and listings accept `--app`
  and `--environment` (also `CHUNK_APP` and `CHUNK_ENVIRONMENT`).

Target selection is saved in `chunk/target.json` under the OS configuration
folder; `CHUNK_CONFIG_DIR` overrides the containing directory. `CHUNK_API_URL`
overrides the saved target for platform commands. Without either, the target is
Chunk Cloud; its API endpoint is not configured yet. Custom URLs may include an
API path and must use HTTP(S) without embedded credentials, queries or fragments.

Authentication, `auth whoami`, and `auth logout` remain stubs. Login saves only the target and
returns success with a "Login coming soon" message.
No credentials are read or stored and no platform requests are made. Future
authentication will use target-scoped OS credential storage, with `CHUNK_API_TOKEN`
as a CI override; that variable is currently unused.

Run `just toolchain` before using the build command from a checkout. `just package-cli`
assembles the CLI and pinned native TypeScript compiler under `target/dist`.
Consumer builds need their project Gradle wrapper and an explicit Java toolchain.
`chunk.toml` and immediate `apps/*/app.toml` files define the project; `chunk dev`
requires `[local]` settings. Local state defaults to `PROJECT/.chunk/local`.
Explicit `--output` and `--state` paths are relative to the working directory.

To package the example without starting services, run `just toolchain`, then
`target/debug/chunk build examples/local`. Its releases appear in
`examples/local/dist`; `just local` builds and runs the same project with state
under `examples/local/.chunk/local`.

Standalone `chunk-backend`, `chunk-control`, `chunk-edge` and `chunk-runtime` binaries
read environment variables and call the same libraries. They have no CLI argument
parser. The proxy remains the reusable listener implementation hosted by edge.
Backend requires `CHUNK_BUNDLE`, `CHUNK_ENVIRONMENT`, `CHUNK_STATE`,
`CHUNK_CONNECTION`, and optional `CHUNK_BIND` (default `127.0.0.1:25568`). See the
[control](crates/chunk-control/README.md), [proxy](crates/chunk-proxy/README.md), and
[runtime](jvm/runtime/README.md) docs for the other service environments.

## License

chunk is licensed under the
[Functional Source License, Version 1.1, MIT Future License](LICENSE.md).
You may use, modify, and redistribute it for any purpose except offering it
as a competing commercial product. Running your own applications on chunk is
not a competing use. Each release converts to the MIT license two years after
it is published.
