# Server declarations

The shared SDK is maintained here as ordinary TypeScript and embedded in the CLI.
This private workspace package is only for internal checks; applications need no
SDK dependency.

Run `chunk codegen PROJECT` after checkout to prepare your editor. It creates
`.chunk/sdk/` (shared implementation), `.chunk/generated/` (schema-bound builders
and types), and these `package.json` imports:

```json
{
  "imports": {
    "#chunk": "./.chunk/generated/index.ts",
    "#chunk/schema": "./.chunk/sdk/schema.ts"
  }
}
```

Commit the package mappings and your `tsconfig.json`; keep `.chunk/` ignored.
Generation preserves unrelated package settings, including import conditions and
their order. It creates an editor `tsconfig.json` only if one is missing, leaving
existing configuration untouched. Existing editor configurations should use
`moduleResolution: "Bundler"`, `allowImportingTsExtensions: true`, `noEmit: true`,
and `lib: ["ES2023"]` for the transactional globals.

Compose the default schema in `server/schema/index.ts` using the independent
schema entry point:

```ts
import { defineSchema, defineTable, v } from "#chunk/schema"

export default defineSchema({
  profiles: defineTable({ player: v.player(), wins: v.integer() })
    .index("by_player", ["player"]),
})
```

Application modules and ordinary shared helpers use schema-bound exports:

```ts
import { query, mutation, v } from "#chunk"
import type { QueryContext, MutationContext, Doc, Id } from "#chunk"

function getProfile(ctx: QueryContext, id: Id<"profiles">): Doc<"profiles"> | null {
  return ctx.db.get("profiles", id)
}

export const wins = query({
  args: { id: v.id("profiles") },
  returns: v.integer(),
  handler: (ctx, { id }) => getProfile(ctx, id)?.wins ?? 0,
})

export const create = mutation({
  args: { player: v.player() }, returns: v.id("profiles"),
  handler: (ctx, { player }) => ctx.db.insert("profiles", { player, wins: 0 }),
})
```

Queries receive a typed Reader, mutations a Writer. `MutationContext` can also be
passed to read helpers. `caller` remains `JsonValue`. Types refer directly to the
schema, so schema edits update editor types without regenerating copied fields.
Use `#chunk/schema` throughout schema modules to keep them independent of the
builders that import that schema.

`chunk build` generates before type-checking. `chunk dev` refreshes the SDK at
startup when the project contains backend sources, then runs the already-built
JVM distribution. It does not watch/rebuild sources or restart deployments; rebuild
the distribution and rerun dev for runtime changes. Rerun `chunk codegen` to repair
missing or stale SDK files. Unchanged generated files are not rewritten.

Result validators are required. `v.optional` means an absent object property;
use `v.union(v.null(), ...)` for explicit null. Numbers must be finite, and integer
values must fit JavaScript's safe range. IDs are branded strings: document IDs
carry their table prefix; player/session IDs have distinct contract types.
IDs describe values and never confer caller authority.

`internalQuery` and `internalMutation` are excluded from public clients. Helpers
may receive the current context to share its transaction. Module initialization
must be pure and context-independent; mutable globals are not database state.

Documents include a readonly `_id`; `v.document(table, fields)` validates returned
documents. Returned values are local copies; use `patch` to persist edits.
IDs contain their table and 128 pseudorandom bits from the invocation's
seeded stream. Repeating the same mutation operation and control flow allocates
the same IDs; a committed retry returns the original outcome. IDs are identifiers,
not secrets. Collisions reject the transaction.

The initial query grammar is ascending named indexes, equality on a contiguous
prefix, then optional `gte`/`lt` bounds on the next field. Document ID breaks ties.
`first()` reads at most one result, `unique()` reads two and rejects duplicates,
and `collect(1..1024)` requires an explicit bound. Read budgets also bound candidate
rows through pending writes. Arbitrary filters, descending order and pagination
are not part of this initial grammar.

Use `unset` to remove optional fields in `patch`; `undefined` is not a deletion
value. Required fields cannot be removed. Fields unknown to a retained deployment
are preserved by the backend when it writes. Ordinary helper calls share the
handler's transaction; any failure discards all its writes.
