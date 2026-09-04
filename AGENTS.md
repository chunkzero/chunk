# chunk

A portable Minecraft server runtime. Rust crates in `crates/` supervise the
server and carry player traffic to it; Kotlin modules in `jvm/` run inside the
Minecraft server's JVM. Hosting backends (cloud sandboxes, containers, plain
Java processes) plug in behind the same runtime. See `README.md` for how the
pieces fit together.

## General guidelines

- Do not edit AGENTS.md or CLAUDE.md unless explicitly asked.
- You may suggest additions to the glossary. Project specific words that take
  time to seek out should be added to the glossary.
- This repository is public and licensed under FSL-1.1-MIT. Do not vendor code
  under incompatible licenses.

## Tooling

- Toolchains are pinned in `mise.toml`; run `mise install` after pulling if it
  changed.
- `just` is the task runner. Prefer its recipes over invoking tools directly:
  - `just fmt` / `just fmt-check` formats or verifies Rust (`rustfmt`) and
    Kotlin (`ktlint`)
  - `just lint` runs Clippy with warnings as errors
  - `just test` runs `cargo test` and `./gradlew test`
  - `just ready` runs everything CI runs; use it before opening a PR
- Rust: a Cargo workspace rooted at `Cargo.toml`. New crates go in `crates/`
  and inherit `workspace.package` and `workspace.lints`.
- JVM: Gradle with the wrapper (`./gradlew`), Kotlin DSL, and the version
  catalog in `gradle/libs.versions.toml`. New modules go in `jvm/`, are
  declared in `settings.gradle.kts`, and apply the `chunk.kotlin-conventions`
  plugin from `buildSrc` instead of repeating toolchain or test setup.
- Do not run the full `just ready` or a full Gradle build to check a single
  edit; run the narrowest command that verifies the change.
- Be careful with destructive actions that are not explicitly asked for by the
  user.

## Code style

- Keep code simple. When implementing a feature, think of `yagni`.
- Type inference is your friend. Don't annotate what the compiler already
  knows.
- Write tests for behavior that matters; don't write endless regression tests
  or smoke tests for every change.
- Keep crate and module APIs narrow and intentional.
- Ensure modularity of code. We don't want 1000+ line monoliths.
- Rust: `unsafe` is forbidden workspace-wide; Clippy `pedantic` is on.
- Kotlin: follow the `ktlint_official` style; warnings are errors.

## Git

- Do not revert unrelated changes.
- Commit messages follow Conventional Commits.
- Squash merge pull requests.
- Do not rely on pre-commit hooks; CI is the source of truth for formatting,
  linting, tests, and builds.

## Glossary

- **Runtime**: the chunk process that runs next to a Minecraft server,
  owns its lifecycle (start, pause, resume, stop, health), and serves the
  tunnel endpoint.
- **Connector**: the chunk process that accepts raw Minecraft TCP connections
  from players and tunnels them to a runtime.
- **Tunnel**: one player's TCP session carried between a connector and a
  runtime.
- **Host**: a backend that provides the machine a runtime runs on and knows
  how to start, pause, and stop it. Examples: a cloud sandbox, a container
  runtime, a local Java process.
- **Pause / resume**: a host-level suspension of an idle server that keeps its
  state, as opposed to a full stop and restart.
