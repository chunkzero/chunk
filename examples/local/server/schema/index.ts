import { defineSchema, defineTable, v } from "#chunk/schema";

export default defineSchema({
  profiles: defineTable({ player: v.player(), coins: v.integer(), visits: v.integer() }).index("by_player", ["player"]),
});
