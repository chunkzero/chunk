# Backend compiler

From the repository, run `pnpm install --frozen-lockfile`, then
`cargo run -p chunk-build --bin chunk-compile -- PROJECT OUTPUT`.
This type-checks the project's `server/**/*.ts` and `apps/*/server/**/*.ts`
and bundles them with Rolldown. The explicitly composed default export in
`server/schema/index.ts` supplies the database schema.

Named function declarations become paths such as `shared/profile/get` and
`apps/duels/match/score`. Helpers remain ordinary TypeScript exports; only
SDK query/mutation declarations enter the contract. Declarations are evaluated
in the bounded transactional engine, without executing project code in Node.
Node builtins, remote imports and native modules are unsupported.

Successful compilation writes `source.mjs`, `source.mjs.map`, and
`contract.json`. No generated client or JVM build is required. `chunk_build::publish`
combines this output with a gameplay distribution and project metadata into an
immutable content-addressed artifact. Compiler dependencies are currently
resolved from this workspace installation.
