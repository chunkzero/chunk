import assert from "node:assert/strict";
import test from "node:test";

import { defineSchema, defineTable, internalMutation, isFunction, query, v } from "../src/index.ts";

test("validators preserve absence, null, safe integers and typed IDs", () => {
  const shape = v.object({ name: v.optional(v.string()), state: v.nullable(v.literal("ready")) });
  assert.deepEqual(shape.parse({ state: null }), { state: null });
  for (const value of [
    { name: null, state: null },
    { name: undefined, state: null },
    { state: "bad" },
    { state: null, extra: true },
  ]) {
    assert.throws(() => shape.parse(value));
  }
  assert.equal(v.integer().parse(Number.MAX_SAFE_INTEGER), Number.MAX_SAFE_INTEGER);
  assert.throws(() => v.number().parse(Number.MAX_SAFE_INTEGER + 1));
  assert.throws(() => v.array(v.number()).parse(Array(2)));
  assert.throws(() => v.object({}).parse(new Date(0)));
  assert.throws(() => v.string().parse("\ud800"));
  assert.equal(v.string().parse("😀"), "😀");
  assert.equal(v.id("profiles").parse("profiles:p1"), "profiles:p1");
  assert.throws(() => v.id("profiles").parse("matches:p1"));
});

test("schema composition is explicit, immutable and collision checked", () => {
  const base = defineTable({ player: v.player(), wins: v.integer(), note: v.optional(v.string()) });
  const profiles = base.index("by_player", ["player"]);
  assert.deepEqual(base.indexes, {});
  const schema = defineSchema({ profiles });
  assert.deepEqual(schema.contract.profiles.indexes, { by_player: ["player"] });
  assert.equal(schema.contract.profiles.fields.player.schema.type, "player");
  assert.throws(() => profiles.index("BY_PLAYER", ["wins"]));
  assert.throws(() => profiles.index("bad", ["missing"]));
  assert.throws(() => defineSchema({ profiles, other: profiles }));
  assert.throws(() => defineSchema({ profiles, Profiles: base }));
  assert.throws(() => {
    schema.contract.profiles.fields.wins.optional = true;
  });
});

test("numbers and numeric literals share safe-integer boundaries", () => {
  for (const value of [Number.MIN_SAFE_INTEGER, Number.MAX_SAFE_INTEGER, 0.125]) {
    assert.equal(v.number().parse(value), value);
    assert.equal(v.literal(value).parse(value), value);
  }
  for (const value of [Number.MIN_SAFE_INTEGER - 1, Number.MAX_SAFE_INTEGER + 1, 1e20, -1e20]) {
    assert.throws(() => v.number().parse(value));
    assert.throws(() => v.literal(value));
  }
  assert.equal(v.literal("100000000000000000000").parse("100000000000000000000"), "100000000000000000000");
});

test("only explicit descriptors are functions and metadata excludes handlers", () => {
  const helper = (x) => x + 1;
  const read = query({ args: { count: v.integer() }, returns: v.integer(), handler: (_, { count }) => helper(count) });
  const internal = internalMutation({ args: {}, returns: v.null(), handler: () => null });
  assert.equal(isFunction(helper), false);
  assert.equal(isFunction(read), true);
  assert.equal(read.handler({}, { count: 2 }), 3);
  assert.equal(internal.contract.visibility, "internal");
  assert.deepEqual(JSON.parse(JSON.stringify(read.contract)), {
    kind: "query",
    visibility: "public",
    arguments: { type: "object", fields: { count: { schema: { type: "integer" }, optional: false } } },
    result: { type: "integer" },
  });
});

test("named unions discriminate objects and API optional nulls normalize without changing database validators", async () => {
  const { apiValidator } = await import("../src/schema.ts");
  const state = v.union({ ready: v.object({}), waiting: v.object({ reason: v.optional(v.string()) }) });
  assert.deepEqual(state.parse({ type: "ready" }), { type: "ready" });
  for (const value of ["ready", {}, { type: "unknown" }, { type: "ready", reason: "extra" }])
    assert.throws(() => state.parse(value));
  assert.throws(() => v.union({ bad: v.string() }));
  assert.throws(() => v.union({ bad: v.object({ type: v.string() }) }));
  assert.throws(() => v.enum("allow", "allow"));
  const value = v.object({ state, note: v.optional(v.string()), result: v.nullable(v.enum("allow", "deny")) });
  const input = { state: { type: "waiting", reason: null }, note: null, result: null };
  assert.throws(() => value.parse(input));
  assert.deepEqual(apiValidator(value).parse(input), { state: { type: "waiting" }, result: null });
  assert.equal(input.note, null);
});
