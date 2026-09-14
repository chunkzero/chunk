import { query, v } from "#chunk";

export const status = query({
  args: { host: v.string() },
  returns: v.serverStatus(),
  handler: () => ({ motd: "My Chunk server", online: 0, max: 16 }),
});

export const admit = query({
  args: v.playerIdentity(),
  returns: v.admissionResult(),
  handler: () => ({ allow: true }),
});

export const route = query({
  args: v.playerIdentity(),
  returns: v.destination(),
  handler: () => ({
    key: "lobby",
    session_type: "lobby/default",
    machine_profile: "local",
  }),
});

export const move = query({
  args: v.playerIdentity().extend({
    destination: v.destination(),
  }),
  returns: v.destination(),
  handler: (_, { destination }) => {
    if (destination.session_type !== "lobby/default" || destination.machine_profile !== "local")
      throw new Error("Unknown destination");
    return destination;
  },
});
