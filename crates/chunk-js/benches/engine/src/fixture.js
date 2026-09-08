export function empty() { return null; }
export async function reads(ctx) {
  let total = 0;
  for (let i = 0; i < 10; i++) total += ctx.db.get("players", String(i)).score;
  return {total};
}
export async function query(ctx, args) {
  const players = ctx.db.scan("players", args?.start ?? "0000", args?.end ?? "0020");
  return players.map(([id, player]) => ({id, score: player.score}))
    .sort((a, b) => b.score - a.score).slice(0, 10);
}
export async function bump(ctx, args) {
  const player = ctx.db.get("players", args.id);
  const score = player.score + 1;
  ctx.db.put("players", args.id, {...player, score});
  return score;
}
export function loop() { while (true) {} }
