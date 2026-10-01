import { createHook, defineScope } from "#chunk";
import { apps } from "#chunk/apps";

export default defineScope({
  hooks: {
    ping: createHook("server.ping", () => ({ motd: "chunk arena | King of the hill", online: 0, max: 100 })),
    route: createHook("player.route", () => apps.lobby.destinations.main),
  },
});
