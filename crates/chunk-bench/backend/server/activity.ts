import { mutation, v } from "#chunk";

// Commits to a table no benchmark query reads, so the log advances while every subscribed result holds.
export const record = mutation({
  args: {},
  returns: v.null(),
  handler: ({ db }) => {
    db.insert("activity", { at: Date.now() });
    return null;
  },
});
