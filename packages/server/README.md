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
