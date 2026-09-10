import { query, v } from "@chunk/server";

export const message = query({
  args: { name: v.string() },
  returns: v.object({ message: v.string() }),
  handler: (_, { name }) => ({ message: `Hello, ${name}!` }),
});
