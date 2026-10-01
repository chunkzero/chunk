import { mutation, query, v } from "#chunk";
import type { JsonValue } from "#chunk";

const identity = v.object({ session: v.session(), app: v.string(), player: v.player() });
const player = (caller: JsonValue) => identity.parse(caller).player;

export const load = query({
  args: {},
  returns: v.object({ saves: v.integer(), playtime: v.integer() }),
  handler: ({ db, caller }) => {
    const profile = db
      .query("profiles")
      .withIndex("by_player", (q) => q.eq("player", player(caller)))
      .unique();
    return { saves: profile?.saves ?? 0, playtime: profile?.playtime ?? 0 };
  },
});

export const save = mutation({
  args: { seconds: v.integer() },
  returns: v.integer(),
  handler: ({ db, caller }, { seconds }) => {
    const id = player(caller);
    const profile = db
      .query("profiles")
      .withIndex("by_player", (q) => q.eq("player", id))
      .unique();
    const saves = (profile?.saves ?? 0) + 1;
    const playtime = (profile?.playtime ?? 0) + seconds;
    if (profile) db.patch(profile._id, { saves, playtime });
    else db.insert("profiles", { player: id, saves, playtime });
    return saves;
  },
});
