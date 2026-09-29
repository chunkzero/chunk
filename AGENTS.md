# chunk

A Minecraft application platform. Gameplay is written as Java or Kotlin apps
on Minestom and backend logic in TypeScript; chunk builds both into one release
and runs it as an environment. Rust crates in `crates/` implement the
environment's services, the edge, the toolchain and the `chunk` CLI. Java
libraries in `jvm/`, with optional Kotlin adapters, run inside each gameplay
JVM. `packages/` holds the self-hosted management service and its dashboard.
See `README.md` for how the pieces fit together.

## General guidelines

- Do not edit AGENTS.md or CLAUDE.md unless explicitly asked. CLAUDE.md only
  imports this file.
- You may suggest additions to the glossary. Project specific words that take
  time to seek out should be added to the glossary.
- This repository is public and licensed under FSL-1.1-MIT. Do not vendor code
  under incompatible licenses.

## Tooling

- Toolchains are pinned in `mise.toml`; run `mise install` after pulling if it
  changed.
- `just` is the task runner. Prefer its recipes over invoking tools directly:
  - `just fmt` / `just fmt-check` formats or verifies Rust (`rustfmt`),
    Kotlin (`ktlint`), Java (`google-java-format --aosp`), Protobuf (Buf),
    TypeScript, JSON, YAML, TOML and Markdown (`oxfmt`), and the justfile
  - `just lint` runs Clippy with warnings as errors, `buf lint` and `oxlint`
  - `just typecheck` type-checks the SDK, management and the dashboard
  - `just test` runs the SDK and dashboard tests, `cargo test`,
    `./gradlew test` and the local example's tests
  - `just ready` runs formatting, lints, type checks, tests, builds and the
    consumer check; use it before opening a PR. CI also runs the management
    tests against Postgres (`pnpm test:management`), `just compose-check`,
    Buf's checks and the container image builds.
- Rust: a Cargo workspace rooted at `Cargo.toml`. New crates go in `crates/`
  and inherit `workspace.package` and `workspace.lints`.
- JVM: Gradle with the wrapper (`./gradlew`), Kotlin DSL, and the version
  catalog in `gradle/libs.versions.toml`. New modules go in `jvm/`, are
  declared in `settings.gradle.kts`, and apply `chunk.java-conventions` (Java
  libraries, most of the JVM code) or `chunk.kotlin-conventions` (Kotlin
  adapters) from `buildSrc`, plus `chunk.publishing-conventions` when
  published, instead of repeating toolchain or test setup. The Gradle plugin in
  `jvm/gradle-plugin` is a separate, included Kotlin build.
- TypeScript: a pnpm workspace. Management runs on Bun and Postgres, the
  dashboard is React on Vite, and the SDK the CLI embeds lives in
  `crates/chunk-build/sdk/`.
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
- Rust: `unsafe` is forbidden workspace-wide, except in `chunk-js`, which
  denies it and allows it only in `src/allocator.rs` and `src/isolate.rs`.
  Clippy `pedantic` is on.
- Java: `google-java-format` AOSP style; compiler warnings are errors.
- Kotlin: follow the `ktlint_official` style; warnings are errors.

## Git

- Do not revert unrelated changes.
- Commit messages follow Conventional Commits.
- Squash merge pull requests.
- Do not rely on pre-commit hooks; CI is the source of truth for formatting,
  linting, tests, and builds.

## Glossary

- **Project**: a directory with `chunk.toml`, its apps under `apps/`, its
  TypeScript backend under `server/`, and assets.
- **App**: a directory under `apps/` whose `app.ts` declares it and its ID,
  with its own Gradle build and executable JAR. Its JVMs run its session types.
- **Session**: one gameplay instance of an app's session type, created by the
  app's `SessionProvider`. A JVM can run several.
- **Release**: the immutable archive `chunk build` produces
  (`dist/<id>.tar.gz`): backend code, app JARs, dependencies and assets. Its
  ID derives from its contents.
- **Environment**: one running instance of a project, such as `prod`, with
  its own database. `chunk-environment` runs its services.
- **Deployment**: a release made current in an environment. The backend keeps
  several deployments against one database.
- **Core**: an environment's authority, the backend and control in one
  process. Gateways, JVMs and the CLI reach it over the `chunk.sync.v1` `Core`
  service.
- **Backend**: the sync engine (`chunk-backend`). It runs the project's
  queries, mutations and actions on embedded V8 over SQLite, with reactive
  subscriptions.
- **Control**: core's placement side (`chunk-control`). It reserves capacity,
  places sessions on hosts and supervises their JVMs.
- **Host**: control's unit of JVM capacity: one capacity request, one JVM
  lifetime, with its own `jvm/<host>` topic. `ProcessHost` runs it as a local
  child process; under management it is a JVM machine running `chunk-jvm`.
  Not a hosting backend; that is a provider.
- **Gateway**: the player listener (`chunk-proxy`). It authenticates players,
  owns encryption and compression, and moves players between sessions and
  JVMs on one connection. It runs next to core or on its own machine.
- **Edge**: `chunk-edge`, in front of a management install. It routes each
  connection by its handshake hostname to an environment's gateway, answers
  server-list pings, and wakes sleeping environments.
- **Management**: the self-hosted control plane (`packages/management`). It
  serves `chunk.management.v1` (projects, environments, releases,
  deployments) and reconciles each environment's machines through a provider.
- **Machine**: what management runs for an environment: its core machine,
  extra gateway machines and JVM machines.
- **Provider**: management's hosting backend. It creates, starts, suspends,
  stops and destroys machines. The Docker/Podman provider ships here; an
  install can plug in its own.
- **Suspend / wake**: management suspends an idle environment's machines once
  core reports it may, keeping their memory where the provider can. A player's
  login through the edge or a due wake alarm resumes it. A suspended
  environment is sleeping.
