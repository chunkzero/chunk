# Backend compiler

Run `chunk codegen PROJECT` to prepare the schema-aware TypeScript SDK for editors without a build or running services.
`chunk gen PROJECT --target java` compiles backend code and generates a Java client. The repository-only
`chunk-compile PROJECT OUTPUT` helper compiles backend artifacts without client generation. Compilation prepares the
SDK, type-checks the project's `server/**/*.ts` and discovered apps' `server/**/*.ts`, then bundles them directly with
the Rust Rolldown API. The explicitly composed default export in `server/schema/index.ts` supplies the database schema.

Named function declarations become paths such as `shared/profile/get` and `apps/duels/match/score`. Helpers remain
ordinary TypeScript exports; only SDK query/mutation declarations enter the contract. Declarations are evaluated in the
bounded transactional engine, without executing project code in Node. Node builtins, remote imports and native modules
are unsupported.

Successful compilation writes `source.mjs`, `source.mjs.map`, and `contract.json`. Backend compilation does not run
Gradle or build JVM apps. `chunk_build::publish_release` combines the backend output with app JARs, shared dependencies,
assets and project metadata into an immutable content-addressed release. Shared SDK sources are embedded in
`chunk-build` and materialized in `PROJECT/.chunk/sdk/`. `PROJECT/.chunk/generated/` contains builders and named
context/document/ID types derived from the project's schema. `package.json` maps `#chunk` and `#chunk/schema` to those
sources; both TypeScript and Rolldown resolve the mappings directly. Schema helpers have no dependency on the
schema-bound builders. Missing/stale files are repaired, unchanged files retain their timestamps, and generated
directories are excluded from source discovery. The default `.chunk/build/` output contains deployment artifacts only.

Generation preserves existing project configuration and merges only its two owned imports. It creates a suitable
`tsconfig.json` if missing. See the [SDK guide](../../packages/server/README.md) for setup and typed helper examples.
The Java, Kotlin and TypeScript client generators remain separate; `#chunk/api` and a TypeScript transport client are
deferred.

`just toolchain` installs native TypeScript 7.0.2 and its library declarations under
`target/debug/toolchain/typescript/7.0.2`. Compilation calls that executable directly, without Node. `CHUNK_TYPESCRIPT`
can select another installation of the same version. `just package-cli` assembles `target/dist/chunk` with the compiler
and license files. Node/pnpm are needed to assemble this distribution, not to run it. SDK type tests remain part of
`pnpm typecheck`.

## Project inspection

`chunk inspect PROJECT` reads `chunk.toml` and sorted immediate `apps/*/app.toml` children, then prints JSON with
`version: 1`, `apps`, and optional `local` settings. Each app entry contains its directory-derived `id`,
project-relative `directory`, Gradle project path such as `:apps:lobby`, and `runtime` requirements. Inspection does not
compile backend sources or run Gradle. App discovery is shared with the backend compiler; unmanifested directories and
nested apps are not included.

Every app needs a regular `build.gradle.kts` beside its `app.toml`. Empty app and project manifests are valid. JVM
toolchains remain explicit in Gradle. For local placement, the root manifest can supply defaults and named machine
profiles:

```toml
[local]
environment = "local"
machine_profile = "local"
capacity = 16
max_processes = 4

[local.profiles.local]
memory_mib = 512
max_sessions = 2
```

An app can override `machine_profile` or `capacity` in `[runtime]`. Inspection resolves root defaults into each app's
runtime metadata and validates profile references. Local capacity is 1–128 players, process count is 1–32, and profiles
allow 128–8192 MiB and 1–16 sessions. No second app list is needed. Unknown fields, including domains and redundant app
names, are rejected.

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
outputs into `dist/<id>/` and `dist/<id>.tar.gz`. It reads the shared app inventory and Gradle's version-1 JSON
descriptor. Every discovered app must have exactly one descriptor entry, an app JAR containing matching
`META-INF/chunk/app.json` metadata, and one `dev.chunkzero.runtime.SessionProvider` service registration. Publication
never runs Java or Gradle. The descriptor’s selected Java executable is used by `chunk dev` unless `--java PATH`
overrides it; the executable must satisfy the release’s Java version requirement. Local state defaults to
`PROJECT/.chunk/local`.

The release includes `source.mjs`, `contract.json`, an optional source map, `backend.json`, `release.json`, and
content-named JARs under `gameplay/lib`. Root `assets/` and discovered apps' `apps/<id>/assets/` retain their paths.
`release.json` records app identities, JAR hashes, resolved dependency coordinates, Java requirements, app
capacity/profile requirements, referenced profile definitions and asset hashes. Original dependency artifact names
distinguish classifier JARs. All JARs share one classpath: conflicting module versions, component/artifact bytes or
effective class definitions fail publication. Multi-release JARs are checked against the selected Java version;
unsupported and preview bytecode is rejected.

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
