import { defineSchema, defineTable, mutation, query, v } from "../src/index.ts"
import type { FunctionDefinition, Id, Infer, PlayerId } from "../src/index.ts"

const profiles = defineTable({ player: v.player(), wins: v.integer() }).index("by_player", ["player"])
defineSchema({ profiles })
// @ts-expect-error index fields must exist
profiles.index("bad", ["missing"])
const read = query({ args: { count: v.integer(), label: v.optional(v.string()) }, returns: v.string(), handler: (ctx, args) => {
  // @ts-expect-error queries cannot write
  ctx.db.put("profiles", "p", {})
  return `${args.count}:${args.label ?? ""}`
} })
mutation({ args: { player: v.player() }, returns: v.null(), handler: (ctx, args) => {
  const player: PlayerId = args.player
  ctx.db.put("profiles", "p", { player })
  return null
} })
query({ args: {}, returns: v.integer(),
  // @ts-expect-error explicit result validator constrains the handler
  handler: () => "wrong",
})
const registered: FunctionDefinition[] = [read]
// @ts-expect-error inferred count must be a number
read.handler({ caller: null, db: { get: () => null, scan: () => [] } }, { count: "wrong" })
const optional = v.object({ value: v.optional(v.string()) })
const absent: Infer<typeof optional> = {}
// @ts-expect-error absence differs from explicit null
const nullable: Infer<typeof optional> = { value: null }
// @ts-expect-error absence differs from explicit undefined
const undefinedValue: Infer<typeof optional> = { value: undefined }
const profile: Id<"profiles"> = v.id("profiles").parse("profiles:p1")
// @ts-expect-error table IDs remain distinct
const match: Id<"matches"> = profile
void [registered, absent, nullable, undefinedValue, match]

const schema = defineSchema({ profiles, matches: defineTable({ score: v.integer() }).index("by_score", ["score"]) })
const { defineFunctions, unset } = await import("../src/index.ts")
const typed = defineFunctions(schema)
typed.mutation({ args: { id: v.id("profiles") }, returns: v.null(), handler: ({ db }, args) => {
  db.patch(args.id, { wins: 2 })
  // @ts-expect-error required fields cannot be removed
  db.patch(args.id, { wins: unset })
  // @ts-expect-error unknown fields cannot be patched
  db.patch(args.id, { typo: 1 })
  // @ts-expect-error table shapes govern insert values
  db.insert("profiles", { wins: "wrong" })
  // @ts-expect-error index must belong to the queried table
  db.query("profiles").withIndex("by_score")
  // @ts-expect-error equality fields must follow the declared index order
  db.query("profiles").withIndex("by_player", q => q.eq("wins", 2))
  return null
} })
typed.query({ args: { id: v.id("matches") }, returns: v.integer(), handler: ({ db }, args) => {
  const doc = db.get(args.id)
  // @ts-expect-error queries cannot write
  db.delete(args.id)
  return doc?.score ?? 0
} })
