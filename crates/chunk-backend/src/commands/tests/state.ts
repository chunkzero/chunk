import { internalMutation, query, sessionMethod, v } from "#chunk";
import type { FunctionReference } from "#chunk";

export const record = internalMutation({
  args: { text: v.string() },
  returns: v.integer(),
  handler: ({ db, caller }, { text }) => {
    db.insert("events", { text, caller: JSON.stringify(caller) });
    return db.query("events").withIndex("by_text").collect(10).length;
  },
});

export const recordRef: FunctionReference<"mutation", { text: string }, number> = {
  path: "shared/state/record",
  kind: "mutation",
  arguments: v.object({ text: v.string() }),
  result: v.integer(),
};

export const read = query({
  args: {},
  returns: v.array(v.object({ text: v.string(), caller: v.string() })),
  handler: ({ db }) =>
    db
      .query("events")
      .withIndex("by_text")
      .collect(10)
      .map(({ text, caller }) => ({ text, caller })),
});

export const population = sessionMethod({
  app: "lobby",
  session: "main",
  name: "population",
  args: { expected: v.integer() },
  returns: v.object({ ready: v.boolean(), total: v.integer() }),
});
