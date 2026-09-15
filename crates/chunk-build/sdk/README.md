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

Attach invocation context with `.withContext(provider)`. It returns a new builder, and providers may be synchronous or
asynchronous. Chained providers run in order and can read fields added by earlier providers. A rejection prevents later
providers and the handler from running.

For example, with a `profiles` table containing `player` and `rank` and a `by_player` index:

```ts
import { query, mutation, v } from "#chunk";
import type { QueryContext } from "#chunk";

const sessionCaller = v.object({
  session: v.session(),
  app: v.string(),
  player: v.optional(v.player()),
});

function playerContext({ caller, db }: QueryContext) {
  const { player } = sessionCaller.parse(caller);
  if (!player) throw new Error("Player context required");
  const profile = db
    .query("profiles")
    .withIndex("by_player", (q) => q.eq("player", player))
    .unique();
  if (!profile) throw new Error("Player profile missing");
  return { player: { id: player, rank: profile.rank } };
}

const playerQuery = query.withContext(playerContext);
const playerMutation = mutation.withContext(playerContext);

export const myRank = playerQuery({
  args: {},
  returns: v.string(),
  handler: ({ player }) => player.rank,
});

const adminQuery = playerQuery.withContext(({ player }) => {
  if (player.rank !== "admin") throw new Error("Admin required");
  return {};
});
```

Player identity comes from the authenticated invocation's `caller`, supplied by session ownership independently of
function arguments. The session caller above includes an optional player handle; proxy and other service callers have
different shapes. Player-required providers reject calls without that context. Ordinary builders and providers that do
not require a player remain usable for those calls.

Providers share the invocation's reader or writer. Query reads become subscription dependencies; mutation reads and
writes share the handler's transaction, including rollback on rejection. Every evaluation runs its providers again,
including query reevaluation after a profile/rank change. Mutation outcome recovery retains its existing deduplication
semantics. The SDK does not cache enriched context across players or calls; keep invocation data out of module globals.

Providers return a plain object containing new fields. They cannot replace `caller`, `db`, or earlier context fields.
Names inherited from `Object.prototype`, such as `toString`, can be added. The context object is frozen, and caller data
is recursively frozen; added values retain their ordinary application semantics. Argument/result inference, query write
restrictions, public/internal visibility and generated client contracts are preserved. Enrichment fields are not
function arguments or results unless explicitly declared. `.withContext()` is also available on raw and internal
builders. It uses the current transactional runtime; external I/O and admission/join lifecycle hooks remain separate
capabilities.

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

Chunk provides reusable schemas for its proxy contracts:

| Validator             | Inferred type     | Fields                                   |
| --------------------- | ----------------- | ---------------------------------------- |
| `v.playerIdentity()`  | `PlayerIdentity`  | `uuid`, `username`                       |
| `v.destination()`     | `Destination`     | `key`, `session_type`, `machine_profile` |
| `v.admissionResult()` | `AdmissionResult` | `allow`, optional `reason`               |
| `v.serverStatus()`    | `ServerStatus`    | `motd`, integer `online` and `max`       |

The validators and types are exported from both `#chunk` and `#chunk/schema`. They compose ordinary object validators,
so they can be nested, extended, or used directly as function arguments and results:

```ts
export const route = query({
  args: v.playerIdentity(),
  returns: v.destination(),
  handler: () => ({
    key: "lobby",
    session_type: "lobby/default",
    machine_profile: "local",
  }),
});

const moveArgs = v.playerIdentity().extend({ destination: v.destination() });
```

These schemas describe values; handlers still decide admission and allowed destinations. Player identity is the proxy's
UUID/username payload, separate from the branded player handle used for backend session callers. Status counts use the
existing integer contract; the proxy additionally requires unsigned 32-bit counts.

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

## Scheduled actions

A mutation can record delayed work in the same commit as document changes:

```ts
import { mutation, v } from "#chunk";

const send = {
  path: "shared/mail/send",
  kind: "action" as const,
  arguments: v.object({ name: v.string() }),
  result: v.null(),
};

export const enqueue = mutation({
  args: { at: v.integer(), name: v.string() },
  returns: v.string(),
  handler: ({ scheduler }, { at, name }) => scheduler.runAt(at, send, { name }),
});
```

The reference must match an `action` or `internalAction` declared at that path. Existing TypeScript client generation
also supplies action references; `chunk codegen` alone materializes the SDK and does not generate those references.
`runAt` takes Unix milliseconds, an action reference and its typed arguments, and returns a `JobId`. The backend
validates the reference and captures caller/deployment identity. A rejected mutation records no job.
`scheduler.cancel(id)` cancels pending work; running work may have partial effects. An explicit
`scheduler.retry(id, at, {acknowledgePossibleEffects: true})` creates a new attempt with the same captured target,
arguments and caller. Retries require a failed, unknown or cancelled job and retained origin code.

Only mutations expose `scheduler`. Queries cannot change scheduling state, and actions must call a mutation to record
intent. Jobs survive backend restart; interrupted attempts become unknown and are never retried automatically. The local
backend dispatches due jobs while running. A hosted environment needs an external alarm adapter to wake from suspension;
the in-process timer cannot do that.
