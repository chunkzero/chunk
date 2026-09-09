import assert from "node:assert/strict"
import test from "node:test"
import { defineSchema, defineTable, internalMutation, isFunction, query, v } from "../src/index.ts"

test("validators preserve absence, null, safe integers and typed IDs", () => {
  const shape = v.object({ name: v.optional(v.string()), state: v.union(v.null(), v.literal("ready")) })
  assert.deepEqual(shape.parse({ state: null }), { state: null })
  for (const value of [{ name: null, state: null }, { name: undefined, state: null }, { state: "bad" }, { state: null, extra: true }]) {
    assert.throws(() => shape.parse(value))
  }
  assert.equal(v.integer().parse(Number.MAX_SAFE_INTEGER), Number.MAX_SAFE_INTEGER)
  assert.throws(() => v.number().parse(Number.MAX_SAFE_INTEGER + 1))
  assert.throws(() => v.array(v.number()).parse(Array(2)))
  assert.throws(() => v.object({}).parse(new Date(0)))
  assert.throws(() => v.string().parse("\ud800"))
  assert.equal(v.string().parse("😀"), "😀")
  assert.equal(v.id("profiles").parse("profiles:p1"), "profiles:p1")
  assert.throws(() => v.id("profiles").parse("matches:p1"))
})

test("schema composition is explicit, immutable and collision checked", () => {
  const base = defineTable({ player: v.player(), wins: v.integer(), note: v.optional(v.string()) })
  const profiles = base.index("by_player", ["player"])
  assert.deepEqual(base.indexes, {})
  const schema = defineSchema({ profiles })
  assert.deepEqual(schema.contract.profiles.indexes, { by_player: ["player"] })
  assert.equal(schema.contract.profiles.fields.player.schema.type, "player")
  assert.throws(() => profiles.index("BY_PLAYER", ["wins"]))
  assert.throws(() => profiles.index("bad", ["missing"]))
  assert.throws(() => defineSchema({ profiles, other: profiles }))
  assert.throws(() => defineSchema({ profiles, Profiles: base }))
  assert.throws(() => { schema.contract.profiles.fields.wins.optional = true })
})

test("only explicit descriptors are functions and metadata excludes handlers", () => {
  const helper = (x) => x + 1
  const read = query({ args: { count: v.integer() }, returns: v.integer(), handler: (_, { count }) => helper(count) })
  const internal = internalMutation({ args: {}, returns: v.null(), handler: () => null })
  assert.equal(isFunction(helper), false)
  assert.equal(isFunction(read), true)
  assert.equal(read.handler({}, { count: 2 }), 3)
  assert.equal(internal.contract.visibility, "internal")
  assert.deepEqual(JSON.parse(JSON.stringify(read.contract)), {
    kind: "query", visibility: "public", arguments: { type: "object", fields: { count: { schema: { type: "integer" }, optional: false } } }, result: { type: "integer" },
  })
})
