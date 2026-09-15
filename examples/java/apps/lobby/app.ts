import { defineApp, v } from "#chunk";

export default defineApp({
  id: "lobby",
  implementations: {
    default: { config: v.object({ greeting: v.string() }) },
  },
});
