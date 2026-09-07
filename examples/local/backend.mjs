export function status(ctx) {
  const settings = ctx.db.get("settings", "server");
  return { motd: settings?.motd ?? "chunk local | Lobby + Arena", online: 0, max: 32 };
}

export function admit(ctx) {
  const mode = ctx.db.get("settings", "server")?.admission ?? "allow";
  if (mode === "hang") while (true) {} // Exercises the bounded hook deadline.
  return { allow: mode !== "deny", reason: "The local example is closed for maintenance." };
}

export function route() {
  return { key: "lobby", session_type: "lobby", machine_profile: "local" };
}

export function move(_ctx, { destination }) {
  if (!["lobby", "arena"].includes(destination.session_type)) throw new Error("Unknown destination");
  return destination;
}

export function balance(ctx, { uuid }) { return ctx.db.get("players", uuid)?.coins ?? 0; }
export function visits(ctx, { uuid }) { return ctx.db.get("players", uuid)?.visits ?? 0; }

export function join(ctx, { uuid }) {
  const player = ctx.db.get("players", uuid) ?? { coins: 0, visits: 0 };
  player.visits++;
  ctx.db.put("players", uuid, player);
  return player.visits;
}

export function coin(ctx, { uuid }) {
  const player = ctx.db.get("players", uuid) ?? { coins: 0, visits: 0 };
  player.coins++;
  ctx.db.put("players", uuid, player);
  return player.coins;
}

export function settings(ctx, value) {
  ctx.db.put("settings", "server", value);
  return null;
}
