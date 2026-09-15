import { createHook, defineScope } from "#chunk";
import { apps } from "#chunk/apps";

export default defineScope({
  hooks: {
    ping: createHook("server.ping", () => ({ motd: "My Chunk server", online: 0, max: 16 })),
    route: createHook("player.route", () => apps.lobby.destinations.main),
  },
});
