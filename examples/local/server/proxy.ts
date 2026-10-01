import { query, v } from "#chunk";

export const status = query({
  args: { host: v.string() },
  returns: v.serverStatus(),
  handler: () => ({ motd: "chunk typed backend | Lobby + Arena", online: 0, max: 32 }),
});
