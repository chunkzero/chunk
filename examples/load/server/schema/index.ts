import { defineSchema, defineTable, v } from "#chunk/schema";

export default defineSchema({
  profiles: defineTable({ player: v.player(), saves: v.integer(), playtime: v.integer() }).index("by_player", [
    "player",
  ]),
});
