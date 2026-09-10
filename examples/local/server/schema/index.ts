import { defineSchema, defineTable, v } from '#chunk/schema';

export default defineSchema({
  profiles: defineTable({ player: v.player(), coins: v.integer(), visits: v.integer() }).index('by_player', ['player']),
  settings: defineTable({ name: v.string(), motd: v.string(), admission: v.union(v.literal('allow'), v.literal('deny')) }).index('by_name', ['name']),
});
