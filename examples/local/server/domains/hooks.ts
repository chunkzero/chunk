import { createHook, v } from "#chunk";

// Function references keep decisions in their own fresh query transaction.
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

export const checkEntry = createHook("player.login", (ctx) => ctx.runQuery(admission, ctx.player));
export const ping = createHook("server.ping", (ctx) => ctx.runQuery(status, { host: ctx.host }));
export const route = createHook("player.route", () => ({
  key: "lobby",
  session_type: "lobby/default",
  machine_profile: "local",
}));
export const beforeMove = createHook("player.beforeMove", ({ destination }) => ({
  allow: ["lobby/default", "arena/default", "arena/large"].includes(destination.session_type),
  reason: "Unknown destination",
}));
