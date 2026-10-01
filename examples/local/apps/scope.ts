import { command, commandArg, createHook, defineScope, v } from "#chunk";
import { apps } from "#chunk/apps";

const status = {
  kind: "query" as const,
  path: "shared/proxy/status",
  arguments: v.object({ host: v.string() }),
  result: v.serverStatus(),
};

export default defineScope({
  hooks: {
    ping: createHook("server.ping", (ctx) => ctx.runQuery(status, { host: ctx.host })),
    route: createHook("player.route", () => apps.lobby.destinations.main),
  },
  commands: {
    hello: command("hello", {
      args: { message: commandArg.greedy() },
      handler: async (ctx, { message }) => {
        await ctx.player.title(`Hello, ${ctx.player.username}`, message);
        await ctx.player.message(message);
      },
    }),
  },
});
