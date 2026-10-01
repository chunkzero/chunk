import { sessionMethod, type SessionMethodReference } from "../src/index.ts";
import { defineDestination, defineSchema, defineTable, mutation, query, v } from "../src/index.ts";
import type { FunctionDefinition, Id, Infer, PlayerId } from "../src/index.ts";

const profiles = defineTable({ player: v.player(), wins: v.integer() }).index("by_player", ["player"]);
defineSchema({ profiles });
// @ts-expect-error index fields must exist
profiles.index("bad", ["missing"]);
const read = query({
  args: { count: v.integer(), label: v.optional(v.string()) },
  returns: v.string(),
  handler: (ctx, args) => {
    // @ts-expect-error queries cannot write
    ctx.db.put("profiles", "p", {});
    return `${args.count}:${args.label ?? ""}`;
  },
});
mutation({
  args: { player: v.player() },
  returns: v.null(),
  handler: (ctx, args) => {
    const player: PlayerId = args.player;
    ctx.db.put("profiles", "p", { player });
    return null;
  },
});
query({
  args: {},
  returns: v.integer(),
  // @ts-expect-error explicit result validator constrains the handler
  handler: () => "wrong",
});
const registered: FunctionDefinition[] = [read];
// @ts-expect-error inferred count must be a number
read.handler({ caller: null, db: { get: () => null, scan: () => [] } }, { count: "wrong" });
const optional = v.object({ value: v.optional(v.string()) });
const absent: Infer<typeof optional> = {};
// @ts-expect-error absence differs from explicit null
const nullable: Infer<typeof optional> = { value: null };
// @ts-expect-error absence differs from explicit undefined
const undefinedValue: Infer<typeof optional> = { value: undefined };
const profile: Id<"profiles"> = v.id("profiles").parse("profiles:p1");
// @ts-expect-error table IDs remain distinct
const match: Id<"matches"> = profile;
void [registered, absent, nullable, undefinedValue, match];

const schema = defineSchema({
  profiles,
  matches: defineTable({ score: v.integer(), tags: v.array(v.string()) }).index("by_score", ["score"]),
});
const { defineFunctions, unset } = await import("../src/index.ts");
const typed = defineFunctions(schema);
typed.mutation({
  args: { id: v.id("profiles") },
  returns: v.null(),
  handler: ({ db }, args) => {
    db.patch(args.id, { wins: 2 });
    // @ts-expect-error required fields cannot be removed
    db.patch(args.id, { wins: unset });
    // @ts-expect-error unknown fields cannot be patched
    db.patch(args.id, { typo: 1 });
    // @ts-expect-error table shapes govern insert values
    db.insert("profiles", { wins: "wrong" });
    // @ts-expect-error index must belong to the queried table
    db.query("profiles").withIndex("by_score");
    // @ts-expect-error equality fields must follow the declared index order
    db.query("profiles").withIndex("by_player", (q) => q.eq("wins", 2));
    return null;
  },
});
typed.query({
  args: { id: v.id("matches") },
  returns: v.integer(),
  handler: ({ db }, args) => {
    const doc = db.get(args.id);
    if (doc) {
      doc.tags.push("local");
      // @ts-expect-error document metadata is readonly
      doc._id = args.id;
    }
    // @ts-expect-error queries cannot write
    db.delete(args.id);
    return doc?.score ?? 0;
  },
});

const named = v.object({ name: v.string(), note: v.optional(v.string()) });
const extended = named.extend({ note: v.integer(), active: v.optional(v.boolean()) });
const extendedValue: Infer<typeof extended> = { name: "Alex", note: 1 };
// @ts-expect-error extension replaces the optional string with a required number
const missingNote: Infer<typeof extended> = { name: "Alex" };
// @ts-expect-error overwritten fields no longer accept their previous type
const stringNote: Infer<typeof extended> = { name: "Alex", note: "old" };
// @ts-expect-error optional fields still distinguish absence from explicit undefined
const undefinedActive: Infer<typeof extended> = { name: "Alex", note: 1, active: undefined };
query({
  args: extended,
  returns: v.integer(),
  handler: (ctx, { name, note, active }) => {
    const optionalBoolean: boolean | undefined = active;
    // @ts-expect-error object arguments do not grant write capabilities to queries
    ctx.db.put("profiles", "p", {});
    // @ts-expect-error object argument fields retain their inferred types
    note.toUpperCase();
    return name.length + note + Number(optionalBoolean);
  },
});
typed.mutation({
  args: v.object({ player: v.player() }),
  returns: v.id("profiles"),
  handler: ({ db }, { player }) => db.insert("profiles", { player, wins: 0 }),
});
query({
  // @ts-expect-error argument validators must describe objects
  args: v.string(),
  returns: v.null(),
  handler: () => null,
});
query({
  args: named,
  returns: v.integer(),
  // @ts-expect-error reusable arguments preserve the explicit return constraint
  handler: () => "wrong",
});
void [extendedValue, missingNote, stringNote, undefinedActive];

const destination: import("../src/index.ts").Destination = v.destination().parse({
  key: "lobby",
  session_type: "lobby/default",
  machine_profile: "local",
});
const identity: import("../src/schema.ts").PlayerIdentity = { uuid: "player-uuid", username: "Alex" };
const admission: import("../src/index.ts").AdmissionResult = { allow: true };
const status: import("../src/index.ts").ServerStatus = { motd: "Server", online: 0, max: 16 };
query({
  args: v.playerIdentity().extend({ destination: v.destination() }),
  returns: v.destination(),
  handler: (_, { uuid, username, destination }) => {
    const names: string[] = [uuid, username, destination.key, destination.session_type, destination.machine_profile];
    // @ts-expect-error identity UUIDs are not branded player handles
    const player: PlayerId = uuid;
    void [names, player];
    return destination;
  },
});
query({
  args: {},
  returns: v.admissionResult(),
  // @ts-expect-error an admission result requires allow
  handler: () => ({ reason: "Denied" }),
});
void [destination, identity, admission, status];

const playerQuery = typed.query.withContext(({ db, caller }) => {
  // @ts-expect-error query providers cannot write
  db.insert("profiles", { player: v.player().parse("alex"), wins: 0 });
  return { player: { id: v.player().parse("alex"), rank: "member" as const }, callerPresent: caller !== null };
});
playerQuery.withContext(({ player }) => ({ allowed: player.rank === "member" }))({
  args: v.object({ name: v.string() }),
  returns: v.string(),
  handler: ({ player, allowed, db, callerPresent }, { name }) => {
    const id: PlayerId = player.id;
    const rank: "member" = player.rank;
    const permission: boolean = allowed && callerPresent;
    // @ts-expect-error enriched queries still cannot write
    db.delete("profiles:alex");
    // @ts-expect-error argument inference survives context composition
    name.toFixed();
    return `${id}:${rank}:${permission}:${name}`;
  },
});
typed.mutation.withContext(({ db }) => ({ id: db.insert("profiles", { player: v.player().parse("alex"), wins: 0 }) }))({
  args: {},
  returns: v.id("profiles"),
  handler: ({ id }) => id,
});
// @ts-expect-error providers cannot replace the trusted caller
query.withContext(() => ({ caller: null }));
// @ts-expect-error providers cannot replace database capabilities
query.withContext(() => ({ db: {} }));
// @ts-expect-error existing enrichment fields cannot be overwritten
playerQuery.withContext(() => ({ player: "other" }));
playerQuery({
  args: {},
  returns: v.integer(),
  // @ts-expect-error enriched builders retain result constraints
  handler: () => "wrong",
});
typed.query({
  args: {},
  returns: v.null(),
  handler: (ctx) => {
    // @ts-expect-error enriching a builder does not modify the original
    void ctx.player;
    return null;
  },
});

interface RankContext {
  rank: string;
}
const rankContext = (): RankContext => ({ rank: "member" });
query.withContext(rankContext)({ args: {}, returns: v.string(), handler: ({ rank }) => rank });

import { createHook } from "../src/index.ts";

createHook(
  "player.login",
  async (ctx) => {
    const identity: string = ctx.player.uuid;
    const candidate: string | undefined = ctx.destination?.session_type;
    void [identity, candidate];
    // @ts-expect-error hooks call transactions rather than capturing a database transaction
    ctx.db.get("profiles", "alex");
    return { allow: true };
  },
  { order: 10 },
);
createHook("server.ping", (ctx) => {
  // @ts-expect-error ping has no connected player
  void ctx.player;
  // @ts-expect-error ping cannot mutate
  void ctx.runMutation;
  return { motd: "Hello", online: 0, max: 16 };
});
// @ts-expect-error login must return an admission decision
createHook("player.login", () => ({ motd: "Hello", online: 0, max: 16 }));
// @ts-expect-error ping has no ordering options
createHook("server.ping", () => ({ motd: "Hello", online: 0, max: 16 }), { order: 1 });
// @ts-expect-error admission cannot opt into following a player
createHook("player.login", () => ({ allow: true }), { followPlayer: true });
// @ts-expect-error event names determine their contexts and results
createHook("arbitrary.event", () => null);
createHook("player.beforeMove", ({ sourceDomain, destination }) => ({ allow: sourceDomain !== destination.key }));
createHook("domain.enter", () => {}, { followPlayer: true });

const { command, commandArg, commandRoute } = await import("../src/commands.ts");
command("reward", {
  args: { amount: commandArg.integer({ min: 1 }), player: commandArg.word() },
  handler: (ctx, args) => {
    const amount: number = args.amount;
    const player: string = args.player;
    // @ts-expect-error command arguments retain parser types
    args.amount.toUpperCase();
    // @ts-expect-error commands do not own a database transaction
    void ctx.db;
    void [amount, player];
  },
});
command("party", {
  routes: [
    commandRoute(["invite"], {
      args: { player: commandArg.word() },
      handler: (_, { player }) => {
        void player.length;
      },
    }),
    commandRoute(["leave"], {
      handler: (_, args) => {
        // @ts-expect-error an argument-free route has no player argument
        void args.player;
      },
    }),
  ],
});

command("immutable", {
  handler: (ctx) => {
    // @ts-expect-error command player identities cannot be changed by handlers
    ctx.player.uuid = "other";
  },
});

const { action } = await import("../src/index.ts");
const countReference = {
  path: "counts/read",
  kind: "query" as const,
  arguments: v.object({ count: v.integer() }),
  result: v.integer(),
};
action({
  args: {},
  returns: v.integer(),
  handler: async (ctx) => {
    // @ts-expect-error actions cannot retain database transactions
    void ctx.db;
    // @ts-expect-error query references cannot invoke mutations
    await ctx.runMutation(countReference, { count: 1 });
    // @ts-expect-error argument types come from the reference
    await ctx.runQuery(countReference, { count: "wrong" });
    const result: number = await ctx.runQuery(countReference, { count: 1 });
    await ctx.sleep(10);
    return result;
  },
});

declare module "../src/functions.ts" {
  interface Vars {
    readonly GREETING: string;
    readonly REGION?: string;
  }
  interface Secrets {
    readonly TOKEN: string;
  }
}

action({
  args: {},
  returns: v.string(),
  handler: async (ctx) => {
    const response = await ctx.fetch("https://example.com/status", {
      method: "POST",
      headers: { authorization: ctx.env.TOKEN },
      body: ctx.env.GREETING,
    });
    // @ts-expect-error HTTP methods are a finite supported set
    await ctx.fetch(new URL("https://example.com/"), { method: "CONNECT" });
    // @ts-expect-error variables only some environments set may be missing
    const region: string = ctx.env.REGION;
    // @ts-expect-error undeclared names are not typed
    void ctx.env.MISSING;
    return response.ok ? `${region}${await response.text()}` : String(response.status);
  },
});
query({
  args: {},
  returns: v.string(),
  handler: (ctx) => {
    // @ts-expect-error transactions cannot access HTTP
    void ctx.fetch;
    // @ts-expect-error transactions cannot read secrets
    void ctx.env.TOKEN;
    return ctx.env.GREETING;
  },
});

const forfeit = sessionMethod({
  app: "duels",
  session: "default",
  name: "forfeit",
  args: { player: v.player() },
  returns: v.boolean(),
});
const typedMethod: SessionMethodReference<{ player: PlayerId }, boolean> = forfeit;
// @ts-expect-error Method arguments retain their wire types.
const wrongMethod: SessionMethodReference<{ player: number }, boolean> = forfeit;
void typedMethod;
void wrongMethod;
const destinationPool = defineDestination({ key: "main", session_type: "lobby/default", machine_profile: "local" });
query({ args: {}, returns: v.destination(), handler: () => destinationPool.destination });
// @ts-expect-error destination identity is immutable
destinationPool.destination.key = "replacement";
// @ts-expect-error groups are not an atomic destination contract
defineDestination({ key: "main", session_type: "lobby/default", machine_profile: "local", group: ["player"] });
// @ts-expect-error supported overflow policies are explicit
defineDestination({ key: "main", session_type: "lobby/default", machine_profile: "local", overflow: "replace" });

const scheduledAction = { ...countReference, kind: "action" as const };
mutation({
  args: {},
  returns: v.string(),
  handler: ({ scheduler }) => {
    // @ts-expect-error only actions can be scheduled
    scheduler.runAt(1, countReference, { count: 1 });
    // @ts-expect-error scheduled arguments retain their reference type
    scheduler.runAt(1, scheduledAction, { count: "wrong" });
    // @ts-expect-error retries must acknowledge possible earlier effects
    scheduler.retry("job", 1, { acknowledgePossibleEffects: false });
    return scheduler.runAt(1, scheduledAction, { count: 1 });
  },
});
query({
  args: {},
  returns: v.null(),
  handler: (ctx) => {
    // @ts-expect-error queries cannot schedule or cancel jobs
    void ctx.scheduler;
    return null;
  },
});
action({
  args: {},
  returns: v.null(),
  handler: (ctx) => {
    // @ts-expect-error actions must call a mutation to record scheduling intent
    void ctx.scheduler;
    return null;
  },
});
mutation({
  args: { player: v.player() },
  returns: v.null(),
  handler: (ctx) => {
    // @ts-expect-error only actions move players
    void ctx.routing;
    return null;
  },
});
query({
  args: {},
  returns: v.null(),
  handler: (ctx) => {
    // @ts-expect-error only actions move players
    void ctx.routing;
    return null;
  },
});
command("travel", {
  handler: async (ctx) => {
    await ctx.routing.enter({ key: "arena", session_type: "arena/default", machine_profile: "small" });
    // @ts-expect-error commands route only their own player
    void ctx.routing.move;
  },
});
createHook("player.connect", (ctx) => {
  // @ts-expect-error only actions move players
  void ctx.routing;
});

import { defineApp, defineScope } from "../src/index.ts";

defineScope({ hooks: { ping: createHook("server.ping", () => ({ motd: "Hi", online: 0, max: 16 })) } });
defineApp({
  id: "arena",
  implementations: { default: { config: v.object({ label: v.string() }) } },
  destinations: {
    small: { implementation: "default", key: "small", maxPlayers: 16, config: { label: "Small" } },
    large: { implementation: "default", key: "large", maxPlayers: 32, config: { label: "Large" } },
    // @ts-expect-error creation configuration is derived from the implementation's validator
    invalid: { implementation: "default", key: "invalid", config: { label: 7 } },
    // @ts-expect-error unknown implementations cannot become destinations
    unknown: { implementation: "missing", key: "missing", config: { label: "Missing" } },
    // @ts-expect-error required configuration cannot be omitted
    absent: { implementation: "default", key: "absent" },
  },
});
defineApp({ id: "lobby", destinations: { main: { implementation: "default", key: "lobby" } } });

import { defineMigration } from "../src/index.ts";

declare module "../src/migrations.ts" {
  interface Migrations {
    "0002_display_name": {
      fighters: {
        old: { readonly _id: Id<"fighters">; name: string };
        row: { readonly _id: Id<"fighters">; displayName: string; title?: string };
        added: { displayName: string; title?: string };
        removed: { name: string };
      };
    };
    "0003_title": {
      fighters: {
        old: { readonly _id: Id<"fighters">; name: string };
        row: { readonly _id: Id<"fighters">; name: string; title: string };
        added: { title: string };
        removed: {};
      };
    };
  }
}
defineMigration("0002_display_name", {
  fighters: { to: (old) => ({ displayName: old.name.trim() }), back: (row) => ({ name: row.displayName }) },
});
defineMigration("0002_display_name", {
  // @ts-expect-error to returns exactly the new fields
  fighters: { to: (old) => ({ displayName: old.name, name: old.name }) },
});
defineMigration("0002_display_name", {
  fighters: {
    to: (old) => ({ displayName: old.name }),
    // @ts-expect-error back returns the removed fields
    back: () => ({}),
  },
});
defineMigration("0002_display_name", {
  // @ts-expect-error old is the previous snapshot's row
  fighters: { to: (old) => ({ displayName: old.displayName }) },
});
// @ts-expect-error migrations transform the tables their snapshots change
defineMigration("0002_display_name", { players: { to: () => ({}) } });
defineMigration("0002_display_name", {
  fighters: {
    // @ts-expect-error every member of a union return has only the new fields
    to: (old): { displayName: string } | { displayName: string; extra: number } => ({ displayName: old.name }),
  },
});
defineMigration("0003_title", {
  fighters: {
    to: () => ({ title: "" }),
    // @ts-expect-error every member of a union return has only the removed fields
    back: (): {} | { extra: number } => ({}),
  },
});
