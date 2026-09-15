import { query, v } from "#chunk";

export const status = query({
  args: { host: v.string() },
  returns: v.serverStatus(),
  handler: ({ db }) => ({
    motd:
      db
        .query("settings")
        .withIndex("by_name", (q) => q.eq("name", "server"))
        .unique()?.motd ?? "chunk typed backend | Lobby + Arena",
    online: 0,
    max: 32,
  }),
});

export const admit = query({
  args: v.playerIdentity(),
  returns: v.admissionResult(),
  handler: ({ db }) => ({
    allow:
      db
        .query("settings")
        .withIndex("by_name", (q) => q.eq("name", "server"))
        .unique()?.admission !== "deny",
    reason: "The local example is closed for maintenance.",
  }),
});
