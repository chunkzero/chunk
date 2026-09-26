import { mutation, query, v } from "#chunk";
import type { Doc, MutationContext, QueryContext } from "#chunk";

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

// Sets a new overall record, so every write changes every leaderboard result regardless of arrival order.
function lead({ db }: MutationContext, found: Doc<"profiles"> | null) {
  if (!found) throw new Error("Profile missing");
  const [leader] = db.query("profiles").withIndex("by_rank").collect(1);
  const best = (leader?.best ?? 0) + 1;
  db.patch(found._id, { best, rank: -best, lastSeen: Date.now() });
  return best;
}

export const submit = mutation({ args: {}, returns: v.integer(), handler: (ctx) => lead(ctx, own(ctx)) });

// As `submit`, for a named player, so callers without a player can raise the leaderboard.
export const raise = mutation({
  args: { player: v.player() },
  returns: v.integer(),
  handler: (ctx, { player }) =>
    lead(
      ctx,
      ctx.db
        .query("profiles")
        .withIndex("by_player", (q) => q.eq("player", player))
        .unique(),
    ),
});
