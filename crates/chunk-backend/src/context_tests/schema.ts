import { defineSchema, defineTable, v } from "#chunk/schema";

export const rank = v.enum("member", "admin");

export default defineSchema({
  profiles: defineTable({ player: v.player(), rank, visits: v.integer() }).index("by_player", ["player"]),
});
