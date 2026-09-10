# Backend compiler

From the repository, run `just toolchain`, then
`cargo run -p chunk-cli -- build PROJECT --output OUTPUT`.
Gradle uses the equivalent `chunk-compile` helper.
This type-checks the project's `server/**/*.ts` and discovered apps' `server/**/*.ts`
and bundles them directly with the Rust Rolldown API. The explicitly composed default export in
`server/schema/index.ts` supplies the database schema.

Named function declarations become paths such as `shared/profile/get` and
`apps/duels/match/score`. Helpers remain ordinary TypeScript exports; only
SDK query/mutation declarations enter the contract. Declarations are evaluated
in the bounded transactional engine, without executing project code in Node.
Node builtins, remote imports and native modules are unsupported.

Successful compilation writes `source.mjs`, `source.mjs.map`, and
`contract.json`. No generated client or JVM build is required. `chunk_build::publish`
combines this output with a gameplay distribution and project metadata into an
immutable content-addressed artifact. The six SDK source files are embedded in `chunk-build` and materialized under
`OUTPUT/.sdk/<digest>` for checking and bundling; modified cache files are rejected.

`just toolchain` installs native TypeScript 7.0.2 and its library declarations under
`target/debug/toolchain/typescript/7.0.2`. Compilation calls that executable directly,
without Node. `CHUNK_TYPESCRIPT` can select another installation of the same version.
`just package-cli` assembles `target/dist/chunk` with the compiler and license files.
Node/pnpm are needed to assemble this distribution, not to run it. SDK type tests
remain part of `pnpm typecheck`.

## Project inspection

`chunk inspect PROJECT` reads `chunk.toml` and sorted immediate `apps/*/app.toml`
children, then prints JSON with `version: 1`, `apps`, and optional `local` settings.
Each app entry contains its directory-derived `id`, project-relative `directory`,
Gradle project path such as `:apps:lobby`, and `runtime` requirements. Inspection
does not compile backend sources or run Gradle. App discovery is shared with the
backend compiler; unmanifested directories and nested apps are not included.

Every app needs a regular `build.gradle.kts` beside its `app.toml`. Empty app and
project manifests are valid. JVM toolchains remain explicit in Gradle. For local
placement, the root manifest can supply defaults and named machine profiles:

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

An app can override `machine_profile` or `capacity` in `[runtime]`. Inspection
resolves root defaults into each app's runtime metadata and validates profile
references. Local capacity is 1–128 players, process count is 1–32, and profiles
allow 128–8192 MiB and 1–16 sessions. No second app list is needed. Unknown fields,
including domains and redundant app names, are rejected.

The backend compiler can discover apps without a root `chunk.toml`. The local
runner still consumes `project.json` and its existing distribution inputs.
