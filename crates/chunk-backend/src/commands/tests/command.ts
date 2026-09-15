import { command, commandArg } from "#chunk";

import { population, recordRef } from "../state.ts";

export const notify = command("notify", {
  args: { text: commandArg.greedy() },
  handler: async (ctx, { text }) => {
    if (ctx.player.uuid !== "alice" || ctx.player.username !== "Alice") throw new Error("Wrong player scope");
    const total = await ctx.runMutation(recordRef, { text });
    const receipt = await ctx.player.message(`${ctx.player.username}: ${text} (#${total})`);
    if (receipt.operationId !== `action/${ctx.invocationId}/platform/2`) throw new Error("Wrong effect identity");
    const status = await ctx.session.call(population, { expected: total });
    if (!status.ready || status.total !== total) throw new Error("Wrong typed session result");
  },
});
