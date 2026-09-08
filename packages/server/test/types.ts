import { defineSchema, defineTable, mutation, query, v } from "../src/index.ts"
import type { Id, Infer, PlayerId } from "../src/index.ts"

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
void [absent, nullable, undefinedValue, match]
