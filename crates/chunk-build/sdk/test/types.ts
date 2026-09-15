import { defineSchema, defineTable, mutation, query, v } from "../src/index.ts";
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
