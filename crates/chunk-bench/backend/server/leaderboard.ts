import { mutation, query, v } from "#chunk";
import type { QueryContext } from "#chunk";

import { own } from "./players.ts";

const entry = v.object({ player: v.player(), name: v.string(), level: v.integer(), best: v.integer() });

function leaders({ db }: QueryContext) {
  return db
    .query("profiles")
    .withIndex("by_rank")
    .collect(10)
    .map(({ player, name, level, best }) => ({ player, name, level, best }));
}

export const top = query({ args: {}, returns: v.array(entry), handler: leaders });

export const standing = query({
  args: {},
  returns: v.object({ top: v.array(entry), me: v.nullable(entry) }),
  handler: (ctx) => {
    const found = own(ctx);
    return {
      top: leaders(ctx),
      me: found && { player: found.player, name: found.name, level: found.level, best: found.best },
    };
  },
});

export const submit = mutation({
  args: { best: v.integer() },
  returns: v.integer(),
  handler: (ctx, { best }) => {
    const found = own(ctx);
    if (!found) throw new Error("Profile missing");
    if (best > found.best) ctx.db.patch(found._id, { best, rank: -best, lastSeen: Date.now() });
    return Math.max(best, found.best);
  },
});
