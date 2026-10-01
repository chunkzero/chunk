import { sessionMethod, v } from "#chunk";

export const matchStatus = sessionMethod({
  app: "arena",
  session: "koth",
  name: "status",
  args: {},
  returns: v.string(),
});
