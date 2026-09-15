import { command, commandArg, createHook, defineScope, v } from "#chunk";
import { apps } from "#chunk/apps";

const admission = {
  kind: "query" as const,
  path: "shared/proxy/admit",
  arguments: v.playerIdentity(),
  result: v.admissionResult(),
};
const status = {
  kind: "query" as const,
  path: "shared/proxy/status",
  arguments: v.object({ host: v.string() }),
  result: v.serverStatus(),
};

export default defineScope({
  hooks: {
    checkEntry: createHook("player.login", (ctx) => ctx.runQuery(admission, ctx.player)),
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
    travel: command("travel", {
      args: { destination: commandArg.word({ suggestions: ["lobby", "arena", "large"] }) },
      handler: async (ctx, { destination }) => {
        const selected =
          destination === "lobby"
            ? apps.lobby.destinations.main
            : destination === "arena"
              ? apps.arena.destinations.standard
              : destination === "large"
                ? apps.arena.destinations.large
                : null;
        if (!selected) {
          await ctx.player.message("Choose lobby, arena or large.");
          return;
        }
        await ctx.routing.enter(selected);
      },
    }),
  },
});
