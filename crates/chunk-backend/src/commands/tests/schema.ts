import { defineSchema, defineTable, v } from "#chunk/schema";

export default defineSchema({
  events: defineTable({ text: v.string(), caller: v.string() }).index("by_text", ["text"]),
});
