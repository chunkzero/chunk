import { query, mutation, internalQuery, internalMutation, unset, v } from "#chunk"
import type { QueryContext, MutationContext, Doc, Id, JsonValue } from "#chunk"

export function getProfile(ctx: QueryContext, id: Id<"profiles">): Doc<"profiles"> | null {
  return ctx.db.get("profiles", id)
}

function addProfile(ctx: MutationContext): Id<"profiles"> {
  return ctx.db.insert("profiles", { wins: 1 })
}

export const wins = query({
  args: { id: v.id("profiles") }, returns: v.integer(),
  handler: (ctx, { id }) => getProfile(ctx, id)?.wins ?? 0,
})
export const create = mutation({ args: {}, returns: v.id("profiles"), handler: addProfile })
export const internalWins = internalQuery({
  args: { id: v.id("profiles") }, returns: v.integer(),
  handler: (ctx, { id }) => getProfile(ctx, id)?.wins ?? 0,
})
export const internalCreate = internalMutation({ args: {}, returns: v.id("profiles"), handler: addProfile })

function checkReader(ctx: QueryContext, id: Id<"profiles">, match: Id<"matches">) {
  const caller: JsonValue = ctx.caller
  const doc = ctx.db.get(id)
  const wins: number | undefined = doc?.wins
  const note: string | undefined = doc?.note
  // @ts-expect-error document IDs are readonly
  if (doc) doc._id = id
  // @ts-expect-error unknown document fields
  doc?.missing
  // @ts-expect-error queries cannot mutate
  ctx.db.insert("profiles", { wins: 0 })
  // @ts-expect-error a query cannot supply a mutation context
  addProfile(ctx)
  // @ts-expect-error table and ID must agree
  ctx.db.get("profiles", match)
  // @ts-expect-error named document types require a known table
  type MissingDoc = Doc<"missing">
  // @ts-expect-error named IDs require a known table
  type MissingId = Id<"missing">
  // @ts-expect-error table IDs remain distinct
  const wrong: Id<"matches"> = id
  // @ts-expect-error unknown tables
  ctx.db.query("missing")
  // @ts-expect-error indexes belong to their table
  ctx.db.query("profiles").withIndex("by_score")
  // @ts-expect-error index keys must follow schema field order
  ctx.db.query("profiles").withIndex("by_wins", q => q.eq("note", "oops"))
  // @ts-expect-error caller has no authenticated identity type yet
  const identity: { player: string } = ctx.caller
  return [caller, wins, note]
}

function checkWriter(ctx: MutationContext, id: Id<"profiles">) {
  const doc: Doc<"profiles"> | null = getProfile(ctx, id)
  ctx.db.patch(id, { wins: 2, note: unset })
  ctx.db.delete(id)
  // @ts-expect-error schema controls inserted values
  ctx.db.insert("profiles", { wins: "wrong" })
  // @ts-expect-error required fields cannot be removed
  ctx.db.patch(id, { wins: unset })
  // @ts-expect-error absent fields differ from explicit undefined
  ctx.db.patch(id, { note: undefined })
  return doc
}

query({ args: {}, returns: v.integer(),
  // @ts-expect-error the result validator constrains the handler
  handler: () => "wrong",
})
internalQuery({ args: {}, returns: v.null(), handler: ctx => {
  // @ts-expect-error internal queries also receive only a Reader
  ctx.db.delete(v.id("profiles").parse("profiles:p1"))
  return null
} })
void [checkReader, checkWriter]
