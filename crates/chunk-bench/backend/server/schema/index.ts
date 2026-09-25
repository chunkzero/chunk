import { defineSchema, defineTable, v } from "#chunk/schema";

export const item = v.object({ item: v.string(), count: v.integer() });

export default defineSchema({
  // Indexes are ascending, so `rank` stores the negated best score for leaderboard order.
  profiles: defineTable({
    player: v.player(),
    name: v.string(),
    coins: v.integer(),
    xp: v.integer(),
    level: v.integer(),
    best: v.integer(),
    rank: v.integer(),
    inventory: v.array(item),
    lastSeen: v.integer(),
    saves: v.integer(),
  })
    .index("by_player", ["player"])
    .index("by_rank", ["rank"]),
});
