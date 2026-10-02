import { command, defineApp, v } from "#chunk";
import { apps } from "#chunk/apps";

import { matchStatus } from "../../server/sessions.ts";

export default defineApp({
  id: "arena",
  runtime: { maxPlayers: 8 },
  worlds: { arena: { source: "worlds/arena.polar" } },
  implementations: {
    koth: { config: v.object({ targetScore: v.integer(), timeLimitSeconds: v.integer() }) },
  },
  destinations: {
    koth: {
      implementation: "koth",
      key: "koth",
      emptyTimeoutSeconds: 30,
      config: { targetScore: 60, timeLimitSeconds: 300 },
    },
  },
  commands: {
    match: command("match", {
      handler: async (ctx) => {
        await ctx.player.message(await ctx.session.call(matchStatus, {}));
      },
    }),
    lobby: command("lobby", {
      handler: async (ctx) => {
        await ctx.routing.enter(apps.lobby.destinations.main);
      },
    }),
  },
});
