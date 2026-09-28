import { mutation, query, v } from "#chunk";
import type { QueryContext } from "#chunk";

import { find } from "./players.ts";

const entry = v.object({ player: v.player(), name: v.string(), level: v.integer(), best: v.integer() });

function leaders({ db }: QueryContext) {
  return db
    .query("profiles")
    .withIndex("by_rank")
    .collect(10)
    .map(({ player, name, level, best }) => ({ player, name, level, best }));
}

// `pad` adds that many bytes to the result, so fan-out can press on core's send budget.
export const top = query({
  args: { pad: v.optional(v.integer()) },
  returns: v.object({ leaders: v.array(entry), pad: v.string() }),
  handler: (ctx, { pad }) => ({ leaders: leaders(ctx), pad: "x".repeat(pad ?? 0) }),
});

// Sets a new overall record for the player, so every write changes every leaderboard result regardless of arrival
// order.
export const raise = mutation({
  args: { player: v.player() },
  returns: v.integer(),
  handler: (ctx, { player }) => {
    const found = find(ctx, player);
    if (!found) throw new Error("Profile missing");
    const [leader] = ctx.db.query("profiles").withIndex("by_rank").collect(1);
    const best = (leader?.best ?? 0) + 1;
    ctx.db.patch(found._id, { best, rank: -best, lastSeen: Date.now() });
    return best;
  },
});
