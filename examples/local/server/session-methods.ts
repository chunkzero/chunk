import { sessionMethod, v } from "#chunk";

export const population = sessionMethod({
  app: "lobby",
  session: "default",
  name: "population",
  args: {},
  returns: v.integer(),
});
