# Backend compiler

From the repository, run `just toolchain`, then
`cargo run -p chunk-cli -- build PROJECT --output OUTPUT`.
Gradle uses the equivalent `chunk-compile` helper.
`chunk codegen PROJECT` prepares the schema-aware SDK for editors without a build
or running services. Build performs the same generation before it type-checks
the project's `server/**/*.ts` and `apps/*/server/**/*.ts` and bundles them directly
with the Rust Rolldown API. The explicitly composed default export in
`server/schema/index.ts` supplies the database schema.

Named function declarations become paths such as `shared/profile/get` and
`apps/duels/match/score`. Helpers remain ordinary TypeScript exports; only
SDK query/mutation declarations enter the contract. Declarations are evaluated
in the bounded transactional engine, without executing project code in Node.
Node builtins, remote imports and native modules are unsupported.

Successful compilation writes `source.mjs`, `source.mjs.map`, and
`contract.json`. No generated client or JVM build is required. `chunk_build::publish`
combines this output with a gameplay distribution and project metadata into an
immutable content-addressed artifact. Shared SDK sources are embedded in `chunk-build`
and materialized in `PROJECT/.chunk/sdk/`. `PROJECT/.chunk/generated/` contains
builders and named context/document/ID types derived from the project's schema.
`package.json` maps `#chunk` and `#chunk/schema` to those sources; both TypeScript
and Rolldown resolve the mappings directly. Schema helpers have no dependency on
the schema-bound builders. Missing/stale files are repaired, unchanged files retain
their timestamps, and generated directories are excluded from source discovery.
The default `.chunk/build/` output contains deployment artifacts only.

Generation preserves existing project configuration and merges only its two owned
imports. It creates a suitable `tsconfig.json` if missing. See the
[SDK guide](../../packages/server/README.md) for setup and typed helper examples.
The existing Java and TypeScript contract-reference generator remains separate;
`#chunk/api` and a TypeScript transport client are deferred.

`just toolchain` installs native TypeScript 7.0.2 and its library declarations under
`target/debug/toolchain/typescript/7.0.2`. Compilation calls that executable directly,
without Node. `CHUNK_TYPESCRIPT` can select another installation of the same version.
`just package-cli` assembles `target/dist/chunk` with the compiler and license files.
Node/pnpm are needed to assemble this distribution, not to run it. SDK type tests
remain part of `pnpm typecheck`.
