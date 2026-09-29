# chunk-build

The toolchain library behind the [`chunk` CLI](../chunk-cli/README.md): it reads a project's manifests, compiles its
TypeScript backend, generates the SDK and backend clients, and publishes and verifies releases. It embeds the TypeScript
SDK, whose [guide](sdk/README.md) covers writing backend code. The Gradle plugin in `jvm/gradle-plugin` calls the CLI
for the parts that need it.

## Project manifest

A project has a `chunk.toml` at its root, its apps under `apps/`, its backend under `server/`, and shared assets under
`assets/`. `chunk.toml` holds the settings for `chunk dev`:

```toml
[local]
environment = "local" # the local environment's name
machine_profile = "local" # the default profile for sessions
capacity = 16 # default players per session, 1 to 128
max_processes = 4 # JVMs at most, 1 to 32
# idle_node_timeout_seconds = 60 # stop a JVM with no unfinished session after this long, 0 to 3600

[local.profiles.local]
memory_mib = 512 # 128 to 8192
max_sessions = 2 # sessions per JVM, 1 to 16
```

Each app is a directory under `apps/` with an `app.ts` and its own `build.gradle.kts`. The app's `id` is stable and
independent of its path: `apps/games/arena/app.ts` may declare `id: "arena"` while Gradle knows it as
`:apps:games:arena`.

```ts
import { defineApp, v } from "#chunk";

export default defineApp({
  id: "arena",
  runtime: { machineProfile: "local", maxPlayers: 16 },
  implementations: { default: { config: v.object({ label: v.string() }) } },
  destinations: {
    standard: { implementation: "default", key: "arena", config: { label: "Arena" } },
    large: { implementation: "default", key: "arena-large", maxPlayers: 32, config: { label: "Large arena" } },
  },
});
```

`implementations` are the app's session types, each backed by a `@SessionType` class in the app's JVM code (omitted, it
is one `default`). `runtime` sets the machine profile and players per session, which an implementation or destination
can override. `destinations` name the sessions other code can send players to, with their creation config.
`apps/scope.ts` and any `scope.ts` in a directory under it declare hooks and commands with `defineScope`; policy
composes down the directory tree. An app's own backend functions live in its `server/` directory. Apps with a legacy
`apps/<name>/app.toml` are still read and belong to the root scope; an app can't have both.

Manifests are read without running any code: the default export must be a direct `defineApp({...})` or
`defineScope({...})` call, and IDs, runtime settings, implementation keys and destination identities must be literals.
`chunk inspect` prints what the tools read, as JSON with `version: 1`, `apps` and `local`. `inspect`, `gen`, `build` and
`dev` need `chunk.toml`; `codegen` doesn't.

## Backend compilation

Compilation prepares the SDK, type-checks the backend (`server/`, each app's `server/`, and the discovered `app.ts` and
`scope.ts` modules) with the pinned native TypeScript 7.0.2, and bundles it with Rolldown. It then evaluates the
declarations in the bounded JavaScript engine to extract the contract; project code never runs under Node, and Node
built-ins, remote imports and native modules are unsupported. The default export of `server/schema/index.ts` defines the
database schema. Exported functions get paths from their file and export name, such as `shared/profile/get` for `get` in
`server/profile.ts`, or `apps/arena/match/score` for `score` in the `arena` app's `server/match.ts`. The output, by
default in `.chunk/build/backend`, is `source.mjs`, its source map and `contract.json`.

`chunk codegen` writes the SDK into `.chunk/sdk/` and the types derived from the project's schema and apps into
`.chunk/generated/`, and maps the `#chunk` imports in `package.json`; it creates `tsconfig.json` if missing and leaves
unchanged files untouched. `chunk gen` compiles and then generates one client:

| Target       | Default output                | Contents                                                                          |
| ------------ | ----------------------------- | --------------------------------------------------------------------------------- |
| `java`       | `.chunk/generated/java`       | `java/<package>/BackendTypes.java` and `java-client/<package>/BackendClient.java` |
| `kotlin`     | `.chunk/generated/kotlin`     | The Java sources plus `kotlin/<package>/CoroutineBackendClient.kt`                |
| `typescript` | `.chunk/generated/typescript` | `api.ts`                                                                          |

The package defaults to `dev.chunkzero.generated`. Each output directory holds a `.chunk-codegen.json` ownership record:
regeneration removes files it generated before, keeps other files, and refuses to overwrite handwritten or edited
generated files. The Gradle plugin runs `chunk gen` before compiling apps, so builds keep clients current.

The TypeScript compiler is looked up at `toolchain/typescript/7.0.2/tsc` beside the CLI (`just toolchain` installs it
into `target/debug`); `CHUNK_TYPESCRIPT` points at another installation of the same version.

## Releases

`chunk build` runs Gradle's `chunkArtifacts`, which compiles the backend and apps and writes a JVM descriptor
(`.chunk/build/jvm/artifacts.json`, version 4) naming each app's executable JAR, dependencies, session types and Java
version. `publish_release(&ReleaseInputs { project, backend, jvm_descriptor, archive }, dist)` then combines the backend
output, app JARs and assets into `dist/<id>/` and, with `archive`, `dist/<id>.tar.gz`. Publication never runs Java or
Gradle. It checks that every discovered app has exactly one descriptor entry whose session types match its
implementations, that each JAR has a valid `Main-Class`, that no two classes on an app's classpath conflict, and that
the bytecode fits the declared Java version.

A release holds:

- `source.mjs`, its source map, `contract.json` and `backend.json`, the backend deployment;
- `apps/<id>/<sha256>.jar`, each app's executable JAR with its own dependencies;
- the project's `assets/` and each app's `assets/`, at their project paths;
- `release.json` (version 3), the manifest control and management read: app identities, JAR hashes, Java version,
  session types with their profile and capacity, profiles and asset hashes.

The release ID is one SHA-256 over the sorted payload names and bytes plus the normalized manifest, so identical inputs
give the same ID and archive bytes on any machine; archives use fixed ordering, permissions and timestamps. Local paths,
the Java executable, environment names, local limits, generated sources and unrelated files such as `.env` are excluded.
Inputs are limited to 4096 files and 256 MiB, and 128 MiB per file; symlinks and ambiguous paths are rejected.
Directories and archives are published atomically and never replace existing content, and files identical to those of an
earlier release in the same directory are hard-linked.

`chunk dev` publishes development releases into `.chunk/local/releases` without an archive. Their apps are thin JARs
behind a small launcher JAR whose `Class-Path` names each dependency under `libs/<sha256>.jar`. Installing a release, on
a JVM machine or in core, verifies it again with the same checks, including that each `Class-Path` entry stays inside
the release and matches its hash.

## Repository tools

`just package-cli` builds the SDK archive, `target/dist/chunk-<version>-linux-x64.tar.gz`, holding the CLI, the
TypeScript compiler and licenses, plus the JVM publications in `target/dist/maven/`; see
[`docs/distribution.md`](../../docs/distribution.md). Two helper binaries serve the repository's own builds:
`chunk-compile PROJECT OUTPUT` compiles a backend without generating clients, and
`chunk-codegen java|kotlin CONTRACT OUTPUT JAVA_PACKAGE` or `chunk-codegen typescript CONTRACT OUTPUT` generates a
client from an existing contract, as the JVM libraries' builds do.

`cargo test -p chunk-build` runs the tests; `just typecheck` covers the SDK's type tests.
