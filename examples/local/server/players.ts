import { defineFunctions, v } from "@chunk/server";
import type { JsonValue } from "@chunk/server";

import schema from "./schema/index.ts";

const { query, mutation } = defineFunctions(schema);
const identity = v.object({ session: v.session(), app: v.string(), player: v.player() });
const player = (caller: JsonValue) => identity.parse(caller).player;
const statistics = v.object({ coins: v.integer(), visits: v.integer() });

export const stats = query({
  args: {},
  returns: statistics,
  handler: ({ db, caller }) => {
    const profile = db
      .query("profiles")
      .withIndex("by_player", (q) => q.eq("player", player(caller)))
      .unique();
    return { coins: profile?.coins ?? 0, visits: profile?.visits ?? 0 };
  },
});

export const join = mutation({
  args: {},
  returns: v.integer(),
  handler: ({ db, caller }) => {
    const id = player(caller);
    const profile = db
      .query("profiles")
      .withIndex("by_player", (q) => q.eq("player", id))
      .unique();
    const visits = (profile?.visits ?? 0) + 1;
    if (profile) db.patch(profile._id, { visits });
    else db.insert("profiles", { player: id, coins: 0, visits });
    return visits;
  },
});

export const coin = mutation({
  args: {},
  returns: v.integer(),
  handler: ({ db, caller }) => {
    const id = player(caller);
    const profile = db
      .query("profiles")
      .withIndex("by_player", (q) => q.eq("player", id))
      .unique();
    const coins = (profile?.coins ?? 0) + 1;
    if (profile) db.patch(profile._id, { coins });
    else db.insert("profiles", { player: id, coins, visits: 0 });
    return coins;
  },
});
