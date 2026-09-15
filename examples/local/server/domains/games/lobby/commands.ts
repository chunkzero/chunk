import { command } from "#chunk";

import { population } from "../../../session-methods.ts";

export const players = command("population", {
  handler: async (ctx) => {
    const count = await ctx.session.call(population, {});
    await ctx.player.message(`This lobby has ${count} player(s).`);
  },
});
