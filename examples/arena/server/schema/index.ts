import { defineSchema, defineTable, v } from "#chunk/schema";

export default defineSchema({
  fighters: defineTable({
    player: v.player(),
    name: v.string(),
    matches: v.integer(),
    wins: v.integer(),
    captures: v.integer(),
    kills: v.integer(),
    deaths: v.integer(),
    // Indexes sort ascending only, so the leaderboard ranks by these negated totals.
    rankWins: v.integer(),
    rankCaptures: v.integer(),
  })
    .index("by_player", ["player"])
    .index("by_rank", ["rankWins", "rankCaptures"]),
});
