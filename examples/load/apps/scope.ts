import { createHook, defineScope } from "#chunk";
import { apps } from "#chunk/apps";

export default defineScope({
  hooks: {
    route: createHook("player.route", () => apps.lobby.destinations.main),
  },
});
