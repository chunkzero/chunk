# Server declarations

`@chunk/server` declares tables and query/mutation contracts. Compose tables
explicitly; exported descriptors register functions during compilation, while
ordinary helpers remain ordinary functions.

```ts
import { defineSchema, defineTable, query, v } from "@chunk/server"

export const schema = defineSchema({
  profiles: defineTable({ player: v.player(), wins: v.integer() })
    .index("by_player", ["player"]),
})

export const greeting = query({
  args: { name: v.string() },
  returns: v.string(),
  handler: (_, { name }) => `Hello, ${name}`,
})
```

Result validators are required. `v.optional` means an absent object property;
use `v.union(v.null(), ...)` for explicit null. Numbers must be finite, and integer
values must fit JavaScript's safe range. IDs are branded strings: document IDs
carry their table prefix; player/session IDs have distinct contract types.
IDs describe values and never confer caller authority.

`internalQuery` and `internalMutation` are excluded from public clients. Helpers
may receive the current context to share its transaction. Module initialization
must be pure and context-independent; mutable globals are not database state.
The typed document API, artifact compiler and generated clients are separate
roadmap changes.

Bind handlers to the explicit schema with `defineFunctions(schema)` to infer the
transaction's document API without generated files:

```ts
const { mutation } = defineFunctions(schema)
export const record = mutation({
  args: { player: v.player() }, returns: v.id("matches"),
  handler: ({ db }, { player }) => {
    const id = db.insert("matches", { player })
    const profile = db.query("profiles")
      .withIndex("by_player", q => q.eq("player", player)).unique()
    if (profile) db.patch(profile._id, { wins: profile.wins + 1 })
    else db.insert("profiles", { player, wins: 1 })
    return id
  },
})
```

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
