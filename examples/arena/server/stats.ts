import { mutation, query, v } from "#chunk";

const team = v.enum("red", "blue");
const arenaSession = v.object({ session: v.session(), app: v.literal("arena") });
const playerCaller = v.object({ session: v.session(), app: v.string(), player: v.player() });

export const recordMatch = mutation({
  args: {
    winner: v.nullable(team),
    players: v.array(
      v.object({
        player: v.player(),
        name: v.string(),
        team,
        captures: v.integer(),
        kills: v.integer(),
        deaths: v.integer(),
      }),
    ),
  },
  returns: v.null(),
  handler: ({ db, caller }, { winner, players }) => {
    arenaSession.parse(caller);
    for (const result of players) {
      const fighter = db
        .query("fighters")
        .withIndex("by_player", (q) => q.eq("player", result.player))
        .unique();
      const wins = (fighter?.wins ?? 0) + (result.team === winner ? 1 : 0);
      const captures = (fighter?.captures ?? 0) + result.captures;
      const totals = {
        name: result.name,
        matches: (fighter?.matches ?? 0) + 1,
        wins,
        captures,
        kills: (fighter?.kills ?? 0) + result.kills,
        deaths: (fighter?.deaths ?? 0) + result.deaths,
        rankWins: -wins,
        rankCaptures: -captures,
      };
      if (fighter) db.patch(fighter._id, totals);
      else db.insert("fighters", { player: result.player, ...totals });
    }
    return null;
  },
});

export const leaderboard = query({
  args: {},
  returns: v.array(v.object({ name: v.string(), wins: v.integer(), captures: v.integer(), kills: v.integer() })),
  handler: ({ db }) =>
    db
      .query("fighters")
      .withIndex("by_rank")
      .collect(10)
      .map(({ name, wins, captures, kills }) => ({ name, wins, captures, kills })),
});

export const mine = query({
  args: {},
  returns: v.object({ matches: v.integer(), wins: v.integer(), captures: v.integer(), kills: v.integer() }),
  handler: ({ db, caller }) => {
    const { player } = playerCaller.parse(caller);
    const fighter = db
      .query("fighters")
      .withIndex("by_player", (q) => q.eq("player", player))
      .unique();
    return {
      matches: fighter?.matches ?? 0,
      wins: fighter?.wins ?? 0,
      captures: fighter?.captures ?? 0,
      kills: fighter?.kills ?? 0,
    };
  },
});
