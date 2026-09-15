import { command, defineApp } from "#chunk";
import { population } from "../../server/session-methods.ts";

export default defineApp({
  id: "lobby",
  runtime: { machineProfile: "local", maxPlayers: 16 },
  destinations: {
    main: { implementation: "default", key: "lobby" },
  },
  commands: {
    players: command("population", {
      handler: async (ctx) => {
        const count = await ctx.session.call(population, {});
        await ctx.player.message(`This lobby has ${count} player(s).`);
      },
    }),
  },
});
