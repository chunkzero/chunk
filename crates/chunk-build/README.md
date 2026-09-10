# Backend compiler

From the repository, run `just toolchain`, then
`cargo run -p chunk-cli -- build PROJECT --output OUTPUT`.
Gradle uses the equivalent `chunk-compile` helper.
This type-checks the project's `server/**/*.ts` and `apps/*/server/**/*.ts`
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
