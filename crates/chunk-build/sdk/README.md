# TypeScript SDK

The SDK a project uses to declare its backend (schema, queries, mutations and actions) and its apps (`app.ts`,
`scope.ts`, hooks, commands, destinations and session methods). Its sources live here as ordinary TypeScript;
`chunk-build` embeds them in the `chunk` CLI, so a project needs no npm dependency and no Node installation.

## In a project

`chunk codegen` writes the SDK into the project's ignored `.chunk/` directory and maps three imports in `package.json`.
`chunk build` and `chunk dev` do the same before every build.

| Import          | Resolves to                 | Use it for                                                |
| --------------- | --------------------------- | --------------------------------------------------------- |
| `#chunk`        | `.chunk/generated/index.ts` | Functions, validators, apps, scopes, hooks and commands   |
| `#chunk/schema` | `.chunk/sdk/schema.ts`      | `defineSchema`, `defineTable` and `v` in `server/schema/` |
| `#chunk/apps`   | `.chunk/generated/apps.ts`  | References to apps' destinations and implementations      |

`#chunk` binds `query`, `mutation`, `QueryContext`, `MutationContext`, `Doc<"table">` and `Id<"table">` to the schema in
`server/schema/index.ts`, so editing the schema updates editor types without regenerating anything. Schema modules
import from `#chunk/schema` instead, because `#chunk` imports the schema.

`codegen` keeps unrelated `package.json` settings and writes a `tsconfig.json` only when none exists. Commit both and
keep `.chunk/` ignored. An existing `tsconfig.json` should use `moduleResolution: "Bundler"`,
`allowImportingTsExtensions: true`, `noEmit: true`, `strict: true`, `exactOptionalPropertyTypes: true`,
`lib: ["ES2023"]` and `types: []`, which is what the build checks with.

A project's backend sources are:

| Path                        | Contents                                                          | Function paths                       |
| --------------------------- | ----------------------------------------------------------------- | ------------------------------------ |
| `server/schema/index.ts`    | `export default defineSchema({...})`                              |                                      |
| `server/**/*.ts`            | Shared functions and helpers                                      | `shared/<file path>/<export>`        |
| `apps/**/app.ts`            | An app: `export default defineApp({...})`                         |                                      |
| `apps/**/scope.ts`          | Hooks and commands for the apps below it; `apps/scope.ts` is root |                                      |
| `apps/<dir>/server/**/*.ts` | Functions local to the app in `apps/<dir>`                        | `apps/<app id>/<file path>/<export>` |

For example, `export const stats` in `server/players.ts` is `shared/players/stats`. `.mts` files work too.

`chunk build` type-checks these sources with the native TypeScript compiler shipped with the CLI, bundles them with
Rolldown into one ES module, and runs that module in chunk's bounded JavaScript engine to extract the contract: tables,
functions, hooks, commands, destinations and session methods. Imports of Node built-ins are rejected. Handlers run on
embedded V8 with the globals in [`web.d.ts`](src/web.d.ts) (`TextEncoder`, `URL`, `crypto`, `console` and a few more),
without Node, filesystem or network access. Module top-level code also runs at build time, so keep it free of side
effects; module globals are not database state.

## Schema

```ts
// server/schema/index.ts
import { defineSchema, defineTable, v } from "#chunk/schema";

export default defineSchema({
  profiles: defineTable({ player: v.player(), coins: v.integer(), visits: v.integer() }).index("by_player", ["player"]),
});
```

Indexes cover up to eight scalar fields (booleans, numbers, strings, IDs, players, sessions and enums). A table has at
most 64 fields and 16 indexes, and a schema at most 128 tables.

Tables are identified by their names in `defineSchema`, not by the files that declare them. Activating a deployment
merges its schema into the environment's database: new tables, new optional fields and new indexes are added, and
tables, fields and indexes the release omits are kept, because older deployments may still use them. Changing an
existing field or index, or adding a required field to an existing table, is rejected.

## Queries and mutations

```ts
// server/players.ts
import { mutation, query, v } from "#chunk";
import type { JsonValue } from "#chunk";

const identity = v.object({ session: v.session(), app: v.string(), player: v.player() });
const player = (caller: JsonValue) => identity.parse(caller).player;

export const stats = query({
  args: {},
  returns: v.object({ coins: v.integer(), visits: v.integer() }),
  handler: ({ db, caller }) => {
    const profile = db
      .query("profiles")
      .withIndex("by_player", (q) => q.eq("player", player(caller)))
      .unique();
    return { coins: profile?.coins ?? 0, visits: profile?.visits ?? 0 };
  },
});

export const coin = mutation({
  args: {},
  returns: v.integer(),
  handler: ({ db, caller }) => {
    const id = player(caller);
    const profile = db
      .query("profiles")
      .withIndex("by_player", (q) => q.eq("player", id))
      .unique();
    const coins = (profile?.coins ?? 0) + 1;
    if (profile) db.patch(profile._id, { coins });
    else db.insert("profiles", { player: id, coins, visits: 0 });
    return coins;
  },
});
```

Queries get a read-only `db` and become reactive subscriptions when watched. Mutations get a writable `db` and a
`scheduler`, and run as one transaction: a thrown error discards every write. `internalQuery` and `internalMutation`
work the same but are left out of generated clients. Helpers can take a `QueryContext` or `MutationContext` to share the
caller's transaction.

`args` is a field map or an object validator; `returns` is required. `caller` is JSON the platform derives from the
authenticated connection, never from arguments; parse it with a validator. Gameplay JVMs call as
`{ session, app, player? }`, gateways (hooks and commands) as `{ kind: "gateway", player? }`, and the CLI as
`{ kind: "cli" }`.

### Validators

| Validator                                    | Value                                                          |
| -------------------------------------------- | -------------------------------------------------------------- |
| `v.null()`, `v.boolean()`, `v.string()`      | The JSON value                                                 |
| `v.number()`, `v.integer()`                  | Finite numbers; integral values must lie within ±(2^53 − 1)    |
| `v.id("table")`, `v.player()`, `v.session()` | Branded ID strings (`Id<"table">`, `PlayerId`, `SessionId`)    |
| `v.literal(x)`, `v.enum("a", "b")`           | One exact value, or one of 1 to 64 identifier strings          |
| `v.optional(x)`, `v.nullable(x)`             | An absent object field, or `null`                              |
| `v.array(x)`, `v.object({...})`              | Arrays and objects; `.extend({...})` returns a new object type |
| `v.union({ ready: v.object({}), ... })`      | A tagged union discriminated by a `type` field                 |
| `v.document("table", {...})`                 | A document with its `_id`                                      |
| `v.playerIdentity()`, `v.destination()`      | `{ uuid, username }`, `{ key, session_type, machine_profile }` |
| `v.admissionResult()`, `v.serverStatus()`    | `{ allow, reason? }`, `{ motd, online, max }`                  |

Use decimal strings for integers outside the safe range; `1e20` is rejected even as a `v.number()`. At the API boundary
an explicit `null` in an optional field is treated as absent. `Infer<typeof validator>` gives a validator's type.

### Documents

- `db.get("table", id)` or `db.get(id)` returns a document or `null`. Documents carry a readonly `_id` and are copies;
  write changes back with `patch`.
- `db.query("table").withIndex("name", (q) => q.eq(...).gte(...).lt(...))` selects by an index: equality on a prefix of
  its fields, then an optional range on the next field, in ascending order with `_id` breaking ties. Finish with
  `first()`, `unique()` (rejects more than one match) or `collect(limit)` with a limit from 1 to 1024. There are no
  filters, descending order or pagination.
- `db.insert("table", value)` returns the new `Id`. IDs are `table:` plus 128 bits from the invocation's seeded random
  stream, so a retried mutation allocates the same IDs. They identify documents; they are not secrets.
- `db.patch(id, fields)` updates fields; set an optional field to `unset` (exported from `#chunk`) to remove it.
  `undefined` does not remove a field. Fields that another retained deployment added are preserved.
- `db.delete(id)` removes a document.

### Context providers

`.withContext(provider)` returns a builder whose handlers receive extra fields. Providers run in order on every
invocation, share its reader or writer, may be async, and can reject the call by throwing:

```ts
import { query, v } from "#chunk";
import type { QueryContext } from "#chunk";

const sessionCaller = v.object({ session: v.session(), app: v.string(), player: v.optional(v.player()) });

function playerContext({ caller, db }: QueryContext) {
  const { player } = sessionCaller.parse(caller);
  if (!player) throw new Error("Player context required");
  const profile = db
    .query("profiles")
    .withIndex("by_player", (q) => q.eq("player", player))
    .unique();
  return { player, coins: profile?.coins ?? 0 };
}

const playerQuery = query.withContext(playerContext);

export const myCoins = playerQuery({
  args: {},
  returns: v.integer(),
  handler: ({ coins }) => coins,
});
```

A provider returns a plain object of new fields; it cannot replace `caller`, `db` or earlier fields. Added fields are
not arguments or results, and nothing is cached between invocations.

## Variables and secrets

`chunk.toml` declares plain variables and the secrets a project needs:

```toml
[vars]
MOTD = "Welcome"

[env.prod.vars]
MOTD = "Welcome to the live server"

[secrets]
required = ["STORE_API_KEY"]
```

Every function reads them as `ctx.env`. `[env.<name>.vars]` overrides `[vars]` in the environment with that name, key by
key: unlike Wrangler, keys it leaves out keep their `[vars]` values. Variable values are plain configuration that lands
in the release, so never put secrets there. Secret values are set per environment with
`chunk secrets put NAME --env ENV`, and `chunk dev` reads them from a gitignored `.dev.vars` in the project root, one
`NAME=value` per line with optional quotes and `#` comments; `chunk dev --env NAME` selects `[env.NAME.vars]`. Queries,
mutations and hooks see the variables only; actions and commands also see every secret of their environment, whose value
is copied in only when read. A secret set while an action runs reaches the actions that start afterwards. Names are
letters, digits and underscores, not starting with a digit, and values are at most 64 KiB. Chunk redacts the secret
values it recognises in logs and error messages on a best-effort basis, but code that logs a secret, even transformed or
nested in other data, can still expose it, so don't log secrets.

`codegen` types `ctx.env` from `chunk.toml` in `.chunk/generated/env.ts`: variables every environment has are strings,
those only some environments set may be missing, and required secrets are strings in actions.

Gameplay code reads the variables, never the secrets, through the generated
[`Vars`](../../../jvm/backend-api/README.md#variables) class, such as `Vars.MOTD`. Each JVM resolves them once, for the
environment its host names in `CHUNK_ENVIRONMENT_NAME`: the environment's own name under management, and `--env NAME`
under `chunk dev`.

```ts
export const checkout = action({
  args: { item: v.string() },
  returns: v.boolean(),
  handler: async (ctx, { item }) => {
    const response = await ctx.fetch("https://store.example.com/v1/checkout", {
      method: "POST",
      headers: { authorization: `Bearer ${ctx.env.STORE_API_KEY}`, "content-type": "application/json" },
      body: JSON.stringify({ item }),
    });
    return response.ok;
  },
});
```

## Actions and function references

`action` and `internalAction` run outside a transaction. Their context has `caller`, `env`, `fetch`, `runQuery`,
`runMutation`, `sleep(ms)`, `invocationId` and `routing`. An action runs for at most 30 seconds. Gameplay JVMs call
public actions through the generated [client](../../../jvm/backend-client/README.md#actions).

`ctx.fetch(url, init)` sends one HTTP request to a public URL, much like the web's `fetch`. `init` takes `method`,
`headers` and a string `body`; the response has `url`, `status`, `ok`, lowercase `headers`, `text()` and `json()`.
Loopback, private and link-local addresses, cloud metadata endpoints included, are refused after DNS resolution and on
every redirect. Bodies are UTF-8 text of up to 64 KiB out and 128 KiB back, a response may carry 64 headers of up to 32
KiB in total, and a fetch has ten seconds. A fetch that was refused, or whose result was lost once it was sent, throws;
a lost result may still have taken effect.

`ctx.routing.move(player, destination)` sends any online player to a destination, through its capacity and overflow
policy like a command's `ctx.routing.enter`:

```ts
const result = await ctx.routing.move(player, apps.arena.destinations.large);
if (result.state === "refused") console.log(result.reason);
```

It resolves to `{ state: "accepted", operationId }` once the move is queued; the player's gateway then carries it out,
and the destination's login and `player.beforeMove` hooks may still turn the player away. Otherwise it resolves to
`{ state: "refused", reason }`, where `reason` is `"offline"`, `"stale"` (the player is still arriving or already
moving), `"full"` (a `"reject"` destination whose one session is full) or `"unknown_destination"` (the release declares
no such destination). Only actions move players; queries, mutations, hooks and commands can't.

`runQuery`, `runMutation`, the scheduler, hooks and command permissions take a function reference: an object with the
function's `path`, `kind` and argument and result validators. Write it next to the code that uses it:

```ts
const admission = {
  kind: "query" as const,
  path: "shared/proxy/admit",
  arguments: v.playerIdentity(),
  result: v.admissionResult(),
};
```

### Scheduled actions

A mutation can schedule an action in the same commit as its writes:

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

`runAt` takes Unix milliseconds and returns a `JobId`; the job keeps the scheduling mutation's caller and deployment. A
rejected mutation schedules nothing. `scheduler.cancel(id)` cancels pending work. Jobs survive restarts, but an attempt
interrupted mid-run becomes unknown and is never retried automatically; retry a failed, unknown or cancelled job with
`scheduler.retry(id, at, { acknowledgePossibleEffects: true })`. Under management, a suspended environment wakes for its
next due job.

## Apps and scopes

An app directory has an `app.ts` and a `build.gradle.kts`. The CLI reads `app.ts` without running it, so the ID, runtime
settings, implementation names and destination keys must be literals; hook and command values may be imported.

```ts
// apps/games/arena/app.ts
import { defineApp, v } from "#chunk";

export default defineApp({
  id: "arena",
  runtime: { machineProfile: "local", maxPlayers: 16 },
  implementations: {
    default: { config: v.object({ label: v.string() }) },
  },
  destinations: {
    standard: { implementation: "default", key: "arena", config: { label: "Arena" } },
    large: {
      implementation: "default",
      key: "arena-large",
      machineProfile: "large",
      maxPlayers: 32,
      config: { label: "Large arena" },
    },
  },
});
```

- `id` is the app's stable identity. Directory names only decide scope nesting and the Gradle project path, so moving an
  app keeps its ID.
- `runtime` sets the default machine profile and players per session (1 to 128). Unset values come from `[local]` in
  `chunk.toml`, and machine profiles must be declared there.
- `implementations` names the app's session types; each key must match exactly one `@SessionType` provider in the app's
  JAR. It defaults to `{ default: {} }`. An implementation with a `config` validator gets a generated JVM provider
  interface that receives the validated value.
- `destinations` are the places players can be sent: an implementation, a `key`, optional `machineProfile`, `maxPlayers`
  and `config`, and a capacity policy. `overflow: "replicate"` (the default) starts another session when every session
  of that destination is full; `"reject"` allows one session and turns further players away. `emptyTimeoutSeconds` (1 to
  86400, default 60) ends a session that has been empty that long. Sessions are created on demand, when a player is
  admitted.

`#chunk/apps` exposes each app's destinations as `{ key, session_type, machine_profile }` references, for example
`apps.arena.destinations.large`, and its implementations as `apps.arena.implementations.default`.

`defineScope({ hooks, commands })` in a `scope.ts` applies to every app below its directory. `defineApp` accepts the
same `hooks` and `commands` maps for one app. Map keys name the handlers, and renaming a key changes its identity. Hook
and command descriptors must be bound in one of these maps; an exported descriptor that is not bound is a build error.

### Hooks

```ts
// apps/scope.ts
import { createHook, defineScope } from "#chunk";
import { apps } from "#chunk/apps";

export default defineScope({
  hooks: {
    ping: createHook("server.ping", () => ({ motd: "My Chunk server", online: 0, max: 16 })),
    route: createHook("player.route", () => apps.lobby.destinations.main),
  },
});
```

| Event                                            | Returns           | Context beyond `eventId`, `domain`, `caller`, `runQuery` |
| ------------------------------------------------ | ----------------- | -------------------------------------------------------- |
| `server.ping`                                    | `ServerStatus`    | `host`                                                   |
| `player.login`                                   | `AdmissionResult` | `player`, `runMutation`, `destination` (or `null`)       |
| `player.route`                                   | `Destination`     | `player`, `runMutation`                                  |
| `player.beforeMove`                              | `AdmissionResult` | `player`, `runMutation`, `sourceDomain`, `destination`   |
| `player.connect`, `domain.enter`, `domain.leave` | nothing           | `player`, `runMutation`                                  |
| `player.disconnect`                              | nothing           | `player`, `runMutation`, `reason`                        |

`server.ping` and `player.route` belong in `apps/scope.ts`, with at most one of each. On login, root `player.login`
hooks run, then routing, then the login hooks of the destination's other ancestor scopes. A move reruns the
destination's login and `player.beforeMove` hooks. Several admission hooks for the same event in one scope need distinct
`order` options (`createHook(event, handler, { order: 1 })`). `player.connect`, `domain.enter` and `domain.leave` accept
`{ followPlayer: true }` to keep running across moves until the player disconnects. Admission has five seconds.

### Commands

```ts
import { command, commandArg } from "#chunk";
import { apps } from "#chunk/apps";

export const travel = command("travel", {
  args: { destination: commandArg.word({ suggestions: ["lobby", "arena"] }) },
  handler: async (ctx, { destination }) => {
    const selected = destination === "lobby" ? apps.lobby.destinations.main : apps.arena.destinations.standard;
    await ctx.routing.enter(selected);
  },
});
```

Bind it in a `commands` map. A command with several subcommands takes
`routes: [commandRoute(["invite"], { args, handler }), ...]` instead of `args` and `handler`. Arguments are
`commandArg.boolean()`, `integer({ min, max })`, `word()`, `string()` (quoted) and `greedy()` (last only). String
arguments take `suggestions`: a list, or a query reference taking `{ input, cursor }` and returning `string[]`. Options
are `aliases`, `permission` (a query reference taking `{}` and returning `boolean`, rechecked on every run) and
`followPlayer`.

Handlers get the action context, without `routing.move`, plus `ctx.player` (`uuid`, `username`, `message(text)`,
`actionBar(text)`, `title(title, subtitle?)`), `ctx.routing.enter(destination)`, and `ctx.session.call(method, args)` or
`ctx.session.send(method, args)` for session methods. Effects resolve to `{ state: "accepted", operationId }`, which
means the gateway accepted them, not that the player saw them. A backend command must not share a name or alias with the
app's JVM commands; on a clash the gateway logs a warning and players see only the JVM's commands.

### Session methods

A session method is a typed call from backend code into a running gameplay session. Export its declaration from any
backend module:

```ts
// server/session-methods.ts
import { sessionMethod, v } from "#chunk";

export const population = sessionMethod({
  app: "lobby",
  session: "default",
  name: "population",
  args: {},
  returns: v.integer(),
});
```

The JVM build generates a `SessionMethods.Lobby.Default.Population` interface for the session class to implement; see
the [Gradle plugin](../../../jvm/gradle-plugin/README.md#session-methods). A command calls it with
`await ctx.session.call(population, {})`, which targets the session the player was in when the command started. The
method runs on that session's tick thread. A lost reply leaves its effect unknown, and nothing retries it automatically.

Destinations that do not belong to an app can be declared with
`defineDestination({ key, session_type, machine_profile })` as named exports of `server/destinations.ts`.

## Developing the SDK

From the repository root, `pnpm exec tsc --project crates/chunk-build/sdk/tsconfig.json` type-checks the SDK, including
the type assertions in [`test/types.ts`](test/types.ts), and `node --test crates/chunk-build/sdk/test/*.test.mjs` runs
its tests. The CLI embeds these files at compile time, so rebuild it (`just toolchain`) to use a change in a project.
