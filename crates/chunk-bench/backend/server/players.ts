import { mutation, query, v } from "#chunk";
import type { JsonValue, QueryContext } from "#chunk";

import { item } from "./schema/index.ts";

const identity = v.object({ session: v.session(), app: v.string(), player: v.player() });
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

export const player = (caller: JsonValue) => identity.parse(caller).player;

export function own({ db, caller }: QueryContext) {
  return db
    .query("profiles")
    .withIndex("by_player", (q) => q.eq("player", player(caller)))
    .unique();
}

export const load = query({
  args: {},
  returns: v.nullable(profile),
  handler: (ctx) => {
    const found = own(ctx);
    if (!found) return null;
    const { _id, rank, ...rest } = found;
    return rest;
  },
});

export const save = mutation({
  args: progress,
  returns: v.integer(),
  handler: (ctx, args) => {
    const found = own(ctx);
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
