import { sessionMethod, v } from "#chunk";

export const announce = sessionMethod({
  app: "lobby",
  session: "default",
  name: "announce",
  args: { message: v.string() },
  returns: v.integer(),
});
