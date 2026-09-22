import { internalQuery, mutation, query, v } from "#chunk";
import type { QueryContext } from "#chunk";

import { rank } from "./schema/index.ts";

const callerSchema = v.object({ session: v.session(), app: v.string(), player: v.optional(v.player()) });

function playerContext({ caller, db }: QueryContext) {
  const { player } = callerSchema.parse(caller);
  if (!player) throw new Error("Player context required");
  const profile = db
    .query("profiles")
    .withIndex("by_player", (q) => q.eq("player", player))
    .unique();
  if (!profile) throw new Error("Player profile missing");
  return { player: { ...profile, id: player } };
}

const playerQuery = query.withContext(playerContext);
const playerMutation = mutation.withContext(playerContext);
const requireAdmin = ({ player }: Awaited<ReturnType<typeof playerContext>>) => {
  if (player.rank !== "admin") throw new Error("Admin required");
  return {};
};

export const profile = playerQuery({
  args: {},
  returns: v.object({ id: v.player(), rank, visits: v.integer() }),
  handler: ({ player }) => ({ id: player.id, rank: player.rank, visits: player.visits }),
});

export const admin = playerQuery.withContext(requireAdmin)({
  args: {},
  returns: v.string(),
  handler: ({ player }) => player.id,
});

export const visit = playerMutation
  .withContext(({ db, player }) => {
    db.patch(player._id, { visits: player.visits + 1 });
    return {};
  })
  .withContext(requireAdmin)({
  args: {},
  returns: v.integer(),
  handler: ({ db, player }) => db.get(player._id)!.visits,
});

export const seed = mutation({
  args: { player: v.player(), rank },
  returns: v.null(),
  handler: ({ db }, args) => {
    db.insert("profiles", { ...args, visits: 0 });
    return null;
  },
});

export const changeRank = mutation({
  args: { player: v.player(), rank },
  returns: v.null(),
  handler: ({ db }, args) => {
    const profile = db
      .query("profiles")
      .withIndex("by_player", (q) => q.eq("player", args.player))
      .unique();
    if (!profile) throw new Error("Player profile missing");
    db.patch(profile._id, { rank: args.rank });
    return null;
  },
});

export const internalProfile = internalQuery.withContext(playerContext)({
  args: {},
  returns: v.string(),
  handler: ({ player }) => player.id,
});
