import { command, commandArg } from "#chunk";

import { arena, lobby } from "../destinations.ts";

export const hello = command("hello", {
  args: { message: commandArg.greedy() },
  handler: async (ctx, { message }) => {
    await ctx.player.title(`Hello, ${ctx.player.username}`, message);
    await ctx.player.message(message);
  },
});

export const travel = command("travel", {
  args: { destination: commandArg.word({ suggestions: ["lobby", "arena"] }) },
  handler: async (ctx, { destination }) => {
    const selected = destination === "lobby" ? lobby : destination === "arena" ? arena : null;
    if (!selected) {
      await ctx.player.message("Choose lobby or arena.");
      return;
    }
    await ctx.routing.enter(selected.destination);
  },
});
