# Backend compiler

Run `chunk codegen PROJECT` to prepare the schema-aware TypeScript SDK for editors without a build or running services.
`chunk gen PROJECT --target java` compiles backend code and generates a Java client. The repository-only
`chunk-compile PROJECT OUTPUT` helper compiles backend artifacts without client generation. Compilation prepares the
SDK, type-checks shared/app-local backend sources and discovered `app.ts`/`scope.ts` modules, then bundles them with the
Rust Rolldown API. The explicitly composed default export in `server/schema/index.ts` supplies the database schema.

Named function declarations become paths such as `shared/profile/get` and `apps/duels/match/score`. Helpers remain
ordinary TypeScript exports; SDK function declarations enter the function contract. App-owned destination declarations
add immutable placement policies and typed creation configuration, validated against the JVM catalog during publication.
Declarations are evaluated in the bounded transactional engine, without executing project code in Node. Node builtins,
remote imports and native modules are unsupported.

Successful compilation writes `source.mjs`, `source.mjs.map`, and `contract.json`. Backend compilation does not run
Gradle or build JVM apps. `chunk_build::publish_release` combines the backend output with app JARs, shared dependencies,
assets and project metadata into an immutable content-addressed release. Shared SDK sources are embedded in
`chunk-build` and materialized in `PROJECT/.chunk/sdk/`. `PROJECT/.chunk/generated/` contains builders and named
context/document/ID types derived from the project's schema. `#chunk/apps` exposes app, implementation, and destination
references discovered without importing executable app modules. `package.json` maps these generated imports to local
sources; both TypeScript and Rolldown resolve the mappings directly. Schema helpers have no dependency on the
schema-bound builders. Missing/stale files are repaired, unchanged files retain their timestamps, and generated
directories are excluded from source discovery. The default `.chunk/build/` output contains deployment artifacts only.

Generation preserves existing project configuration and merges only its owned imports. It creates a suitable
`tsconfig.json` if missing. See the [SDK guide](sdk/README.md) for setup and typed helper examples. The Java, Kotlin and
TypeScript client generators remain separate; `#chunk/api` and a TypeScript transport client are deferred.

`just toolchain` installs native TypeScript 7.0.2 and its library declarations under
`target/debug/toolchain/typescript/7.0.2`. Compilation calls that executable directly, without Node. `CHUNK_TYPESCRIPT`
can select another installation of the same version. `just package-cli` assembles `target/dist/chunk` with the compiler
and license files. Node/pnpm are needed to assemble this distribution, not to run it. SDK type tests remain part of
`pnpm typecheck`.

## Project inspection

`chunk inspect PROJECT` reads `chunk.toml` and recursive `apps/**/app.ts` declarations, then prints JSON with
`version: 1`, `apps`, and optional `local` settings. Every app declares a stable `id`; its physical path supplies the
Gradle project path and scope ancestry. `apps/games/arena/app.ts` can declare `id: "duels"` while Gradle uses
`:apps:games:arena`. Each app needs a regular `build.gradle.kts` beside its declaration; toolchains and dependencies
stay in Gradle.

```ts
import { defineApp, v } from "#chunk";

export default defineApp({
  id: "arena",
  runtime: { machineProfile: "small", maxPlayers: 16 },
  implementations: { default: { config: v.object({ label: v.string() }) } },
  destinations: {
    standard: { implementation: "default", key: "standard", config: { label: "Standard" } },
    large: { implementation: "default", key: "large", maxPlayers: 32, config: { label: "Large" } },
  },
});
```

Inspection parses TypeScript syntax without executing declarations or resolving imports. The default export must be a
direct `defineApp({...})` or `defineScope({...})` call. IDs, runtime requirements, implementation keys, and destination
identities use literals. Hook/command maps have literal keys and can reference imported descriptors. Configuration
validators and creation values are evaluated later during bounded backend compilation. Spreads, computed keys, and
computed runtime requirements cannot participate in static metadata.

`apps/scope.ts` supplies root policy. Descendant `scope.ts` files and app-local `hooks`/`commands` compose policy by
physical directory ancestry; intermediate directories are implicit scopes. Apps do not repeat a domain backlink.
`server/schema/index.ts` still explicitly composes database schemas, and app-local backend functions remain under each
app's `server/` directory. Generated `#chunk/apps` references exist before backend and JVM compilation.

Root local defaults remain in `chunk.toml`:

```toml
[local]
environment = "local"
machine_profile = "small"
capacity = 16
max_processes = 4

[local.profiles.small]
memory_mib = 512
max_sessions = 2
```

App runtime defaults and optional implementation `runtime` overrides use `machineProfile` and `maxPlayers`. Inspection
emits the existing `machine_profile`/`capacity` fields for build and placement tools. Destinations can override those
defaults while retaining the same implementation. Omitted `implementations` means the ordinary `default` provider;
omitted implementation `config` accepts only `{}` and requires no generated configuration provider interface.

Local capacity is 1–128 players, process count is 1–32, and profiles allow 128–8192 MiB and 1–16 sessions. Scopes come
only from `apps/**/scope.ts` and `app.ts` directories; a `server/domains` tree is rejected with a migration diagnostic.
Legacy immediate `apps/*/app.toml` apps remain supported and bind to the root scope. An app cannot contain both `app.ts`
and `app.toml`.

The backend compiler can discover apps without a root `chunk.toml`. Public CLI commands require the root manifest.
`chunk dev PROJECT` uses its local settings and each discovered app’s resolved requirements for session placement.

## Complete releases

`chunk build PROJECT` validates project metadata, runs the project’s Gradle wrapper with `chunkArtifacts`, and publishes
the complete release into `PROJECT/dist`. Gradle invokes `chunk gen` before compiling app code and writes
`.chunk/build/jvm/artifacts.json`; the CLI supplies its own executable with `-Pchunk.executable` so generation uses the
same installation. `--output PATH` selects a release directory relative to the current working directory. Failed or
cancelled Gradle builds stop their child processes and do not publish a release. From a checkout, `just toolchain`
followed by `target/debug/chunk build examples/local` builds the two-app example without starting services. Its archive
and release directory appear under `examples/local/dist`.

`publish_release(&ReleaseInputs { project, backend, jvm_descriptor }, dist)` combines separately built backend and JVM
outputs into `dist/<id>/` and `dist/<id>.tar.gz`. It reads the shared app inventory and Gradle's version-3 JSON
descriptor. Every discovered app must have exactly one descriptor entry containing its session type IDs and an
executable JAR with a valid `Main-Class`. Publication never runs Java or Gradle. The descriptor’s selected Java
executable is used by `chunk dev` unless `--java PATH` overrides it; the executable must satisfy the release’s Java
version requirement. Local state defaults to `PROJECT/.chunk/local`.

The release includes `source.mjs`, `contract.json`, an optional source map, `backend.json`, `release.json`, and
content-named executable JARs under `apps/<id>/`. Root `assets/` and discovered apps' `apps/<id>/assets/` retain their
paths. `release.json` records app identities, JAR hashes, Java requirements, session type IDs with resolved profile and
capacity settings, referenced profile definitions and asset hashes. It is the deployment manifest consumed by control;
JARs contain only executable code, dependencies and their local factory registries. Each app carries its own
dependencies and may use different dependency versions. Publication validates executable entrypoints and effective
multi-release bytecode against the declared Java version. Descriptor and release metadata use version 3; older artifacts
must be rebuilt.

The descriptor's absolute file paths and Java executable are local build inputs. They are excluded from the release,
along with environment names, local process limits, `.sdk` caches, generated sources, project build files and unrelated
files such as `.env`. Only explicitly supplied asset directories are collected. Inputs are bounded to 4096 payload files
and 256 MiB in total; individual files are limited to 128 MiB. Symlinks and ambiguous portable paths are rejected.

One digest covers sorted payload names and bytes plus normalized release metadata before its ID is added. `backend.json`
is derived afterward; both JSON manifests carry that same deployment ID. The archive wrapper does not participate in
identity. Tar entries use stable ordering, permissions and timestamps, including deterministic long-path records; gzip
headers contain no local filename or timestamp. Moving identical inputs to another machine does not change the ID or
archive bytes. Directories and archives are each published atomically without replacing existing content. Reuse verifies
their bytes and rejects tampering. Archive upload and deployment promotion remain separate operations.

## Client generation

An installed `chunk` CLI can compile the backend and generate one selected client language without invoking Gradle:

```sh
chunk gen . --target java --java-package com.example.backend
chunk gen . --target kotlin --java-package com.example.backend
chunk gen . --target typescript
```

Generation validates the same `chunk.toml` and discovered app metadata as `chunk inspect`. Java output defaults to
`.chunk/generated/java`, containing `java/<package>/BackendTypes.java` and `java-client/<package>/BackendClient.java`.
Add both source roots to a Java consumer; the Java package defaults to `dev.chunkzero.generated`. TypeScript output
defaults to `.chunk/generated/typescript/api.ts`, retaining nested references and document validators from the compiled
contract. Java generation does not emit TypeScript, and TypeScript generation does not require a Java package or JVM
tools.

Kotlin output defaults to `.chunk/generated/kotlin` and contains the same Java model/client source roots plus
`kotlin/<package>/CoroutineBackendClient.kt`. Compile all three source roots with `backend-client-kotlin` on the
classpath. The facade borrows an existing owned `CoroutineBackend` and uses the shared Java records, Jackson bindings,
and references. Java-only generation adds no Kotlin sources or dependencies.

```kotlin
val playerBackend = CoroutineBackendClient(ownedPlayerBackend)
val stats = playerBackend.shared.players.stats()
playerBackend.shared.players.coin(operation = reward)
playerBackend.shared.players.watchStats().collect { state -> /* full WatchState */ }
```

Queries and mutations suspend in the caller's context. Mutation IDs remain explicit and reusable after an unknown
outcome. Watches are cold `Flow<WatchState<R>>` values, retaining stale flags, snapshot revisions, query failures, and
transport errors. Cancelling a call or collection cancels its RPC or watch; closing the adapter ends its calls and
watches. The facade creates no scope and does not own the supplied adapter.

Java callers use grouped methods such as `playerBackend.shared().players().stats()` and `coin(operation)`, plus
closeable `watchStats(observer)` subscriptions carrying the full watch state. Empty-object arguments have no-argument
conveniences; typed overloads and generic references remain available. Public models follow the same hierarchy, for
example `BackendTypes.Shared.Players.StatsResult`.

`--output PATH` overrides the selected client directory. `--backend-output PATH` overrides the compiler output, whose
default is `.chunk/build/backend`. Defaults are relative to the project; explicit paths are relative to the working
directory. The two output directories must be separate, with neither containing the other. Compiler output contains only
the executable `source.mjs`, source map and `contract.json`. The editor SDK lives in the project's `.chunk/sdk` and
`.chunk/generated/index.ts`. Client directories contain only the selected sources and `.chunk-codegen.json`, the
generator's ownership record. Generated sources and ownership records are not release artifacts.

Regeneration removes stale files recorded in the destination's ownership record, including old Java package paths. It
preserves other files and rejects collisions with handwritten files or modifications to previously generated files. Keep
the ownership record alongside generated sources; moving an edited generated file aside allows regeneration. Outputs
from older generators without an ownership record should use a fresh destination. Each destination belongs to one
target; selecting a different target there replaces its previously generated files.

Repository fixtures can generate directly from an existing contract:

```sh
cargo run -p chunk-build --bin chunk-codegen -- java CONTRACT OUTPUT JAVA_PACKAGE
cargo run -p chunk-build --bin chunk-codegen -- kotlin CONTRACT OUTPUT JAVA_PACKAGE
cargo run -p chunk-build --bin chunk-codegen -- typescript CONTRACT OUTPUT
```
