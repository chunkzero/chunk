import { mutation, query, v } from "#chunk";
import type { PlayerId, QueryContext } from "#chunk";

import { item } from "./schema/index.ts";

const progress = {
  coins: v.integer(),
  xp: v.integer(),
  level: v.integer(),
  best: v.integer(),
  inventory: v.array(item),
};
const profile = v.object({
  player: v.player(),
  name: v.string(),
  ...progress,
  lastSeen: v.integer(),
  saves: v.integer(),
});

export function find({ db }: QueryContext, player: PlayerId) {
  return db
    .query("profiles")
    .withIndex("by_player", (q) => q.eq("player", player))
    .unique();
}

export const load = query({
  args: { player: v.player() },
  returns: v.nullable(profile),
  handler: (ctx, { player }) => {
    const found = find(ctx, player);
    if (!found) return null;
    const { _id, rank: _rank, ...rest } = found;
    return rest;
  },
});

export const save = mutation({
  args: { player: v.player(), ...progress },
  returns: v.integer(),
  handler: (ctx, { player, ...args }) => {
    const found = find(ctx, player);
    if (!found) throw new Error("Profile missing");
    const saves = found.saves + 1;
    ctx.db.patch(found._id, { ...args, rank: -args.best, lastSeen: Date.now(), saves });
    return saves;
  },
});

export const seed = mutation({
  args: { first: v.integer(), count: v.integer() },
  returns: v.integer(),
  handler: ({ db }, { first, count }) => {
    for (let index = first; index < first + count; index++) {
      const best = (index * 7919) % 100_000;
      db.insert("profiles", {
        player: v.player().parse(`p${index}`),
        name: `Player ${index}`,
        coins: index % 1000,
        xp: index * 10,
        level: 1 + (index % 50),
        best,
        rank: -best,
        inventory: Array.from({ length: 8 }, (_, slot) => ({ item: `item-${(index + slot) % 64}`, count: 1 + slot })),
        lastSeen: 0,
        saves: 0,
      });
    }
    return count;
  },
});
