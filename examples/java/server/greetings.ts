import { query, v } from "#chunk";

export const message = query({
  args: { name: v.string() },
  returns: v.object({ message: v.string() }),
  handler: (_, { name }) => ({ message: `Hello, ${name}!` }),
});
