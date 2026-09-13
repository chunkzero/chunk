import { query, v } from "#chunk";

const user = { uuid: v.string(), username: v.string() };
const destination = v.object({ key: v.string(), session_type: v.string(), machine_profile: v.string() });

export const status = query({
  args: { host: v.string() },
  returns: v.object({ motd: v.string(), online: v.integer(), max: v.integer() }),
  handler: () => ({ motd: "My Chunk server", online: 0, max: 16 }),
});
export const admit = query({
  args: user,
  returns: v.object({ allow: v.boolean(), reason: v.string() }),
  handler: () => ({ allow: true, reason: "" }),
});
export const route = query({
  args: user,
  returns: destination,
  handler: () => ({ key: "lobby", session_type: "lobby/default", machine_profile: "local" }),
});
export const move = query({
  args: { ...user, destination },
  returns: destination,
  handler: (_, { destination }) => {
    if (destination.session_type !== "lobby/default" || destination.machine_profile !== "local")
      throw new Error("Unknown destination");
    return destination;
  },
});
