# Server declarations

The shared SDK is maintained here as ordinary TypeScript and embedded in the CLI by `chunk-build`. Applications need no
SDK dependency. Run `pnpm typecheck` and `pnpm test` from the repository root to check the SDK types and behavior.

Run `chunk codegen PROJECT` after checkout to prepare your editor. It creates `.chunk/sdk/` (shared implementation),
`.chunk/generated/` (schema-bound builders and types), and these `package.json` imports:

```json
{
  "imports": {
    "#chunk": "./.chunk/generated/index.ts",
    "#chunk/schema": "./.chunk/sdk/schema.ts"
  }
}
```

Commit the package mappings and your `tsconfig.json`; keep `.chunk/` ignored. Generation preserves unrelated package
settings, including import conditions and their order. It creates an editor `tsconfig.json` only if one is missing,
leaving existing configuration untouched. Existing editor configurations should use `moduleResolution: "Bundler"`,
`allowImportingTsExtensions: true`, `noEmit: true`, and `lib: ["ES2023"]` for the transactional globals.

Compose the default schema in `server/schema/index.ts` using the independent schema entry point:

```ts
import { defineSchema, defineTable, v } from "#chunk/schema";

export default defineSchema({
  profiles: defineTable({ player: v.player(), wins: v.integer() }).index("by_player", ["player"]),
});
```

Application modules and ordinary shared helpers use schema-bound exports:

```ts
import { query, mutation, v } from "#chunk";
import type { QueryContext, MutationContext, Doc, Id } from "#chunk";

function getProfile(ctx: QueryContext, id: Id<"profiles">): Doc<"profiles"> | null {
  return ctx.db.get("profiles", id);
}

export const wins = query({
  args: { id: v.id("profiles") },
  returns: v.integer(),
  handler: (ctx, { id }) => getProfile(ctx, id)?.wins ?? 0,
});

export const create = mutation({
  args: { player: v.player() },
  returns: v.id("profiles"),
  handler: (ctx, { player }) => ctx.db.insert("profiles", { player, wins: 0 }),
});
```

Queries receive a typed Reader, mutations a Writer. `MutationContext` can also be passed to read helpers. `caller`
remains `JsonValue`. Types refer directly to the schema, so schema edits update editor types without regenerating copied
fields. Use `#chunk/schema` throughout schema modules to keep them independent of the builders that import that schema.

`chunk build` and `chunk dev` generate the SDK before type-checking and building the complete app release. Dev runs that
release; it does not watch sources or restart automatically. Rerun dev after runtime changes. Rerun `chunk codegen` to
repair missing or stale SDK files. Unchanged generated files are not rewritten.

Function arguments accept either a field map or a reusable object validator. Object validators can also be nested or
used as results:

```ts
const named = v.object({ name: v.string() });

export const greeting = query({
  args: named,
  returns: v.string(),
  handler: (_, { name }) => `Hello, ${name}!`,
});

const withNickname = named.extend({ nickname: v.optional(v.string()) });
```

`.extend()` returns a new validator. Exact field names replace their previous validators, including required/optional
status; differently cased collisions are rejected. The original validator and existing field-map declarations remain
unchanged. Function arguments must describe an object; arrays, nullable objects and primitive validators are rejected.

Result validators are required. `v.optional` allows an absent object property; `v.nullable(...)` allows explicit null.
At the API boundary, optional properties normalize explicit null to omission. Database values and patches retain
explicit presence semantics. Use `v.enum("allow", "deny")` for string choices and
`v.union({ready: v.object({}), waiting: v.object({reason: v.string()})})` for tagged unions with a `type` discriminator.
Numbers must be finite. All integral values must lie between `-(2^53 - 1)` and `2^53 - 1`, including values declared
with `v.number()` or represented as floating-point JSON numbers. For example, `1e20` is rejected even though it is a
finite JavaScript number. Use decimal strings for larger integral values, such as `"100000000000000000000"`. The same
rule applies to numeric literals in table fields, function arguments and results, and generated Java/Kotlin clients.

IDs are branded strings: document IDs carry their table prefix; player/session IDs have distinct contract types. IDs
describe values and never confer caller authority.

`internalQuery` and `internalMutation` are excluded from public clients. Helpers may receive the current context to
share its transaction. Module initialization must be pure and context-independent; mutable globals are not database
state.

Documents include a readonly `_id`; `v.document(table, fields)` validates returned documents. Returned values are local
copies; use `patch` to persist edits. IDs contain their table and 128 pseudorandom bits from the invocation's seeded
stream. Repeating the same mutation operation and control flow allocates the same IDs; a committed retry returns the
original outcome. IDs are identifiers, not secrets. Collisions reject the transaction.

The initial query grammar is ascending named indexes, equality on a contiguous prefix, then optional `gte`/`lt` bounds
on the next field. Document ID breaks ties. `first()` reads at most one result, `unique()` reads two and rejects
duplicates, and `collect(1..1024)` requires an explicit bound. Read budgets also bound candidate rows through pending
writes. Arbitrary filters, descending order and pagination are not part of this initial grammar.

Use `unset` to remove optional fields in `patch`; `undefined` is not a deletion value. Required fields cannot be
removed. Fields unknown to a retained deployment are preserved by the backend when it writes. Ordinary helper calls
share the handler's transaction; any failure discards all its writes.
