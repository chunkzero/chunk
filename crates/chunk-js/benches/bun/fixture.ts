// Mirrors crates/chunk-js/benches/engine/src/main.rs fixture(): the same handler
// text and synthetic helper declarations, so bundle bytes match across engines.
const HANDLERS = `export function empty() { return null; }
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
`;

export function fixture(kib: number, init: "eager" | "declarations"): string {
  let source = HANDLERS;
  let count = 0;
  while (source.length < kib * 1024) {
    source += `function rule${count}(x) { return {score: x.score + ${count}, online: x.online, tag: 'rule-${count}'}; }\n`;
    count += 1;
  }
  if (count > 0) {
    source += "const rules = [";
    for (let i = 0; i < count; i++) source += `rule${i},`;
    source += "];\n";
    if (init !== "declarations") {
      source += "const defaults = rules.map(rule => rule({score: 1, online: true}));\n";
      source += "if (defaults.length !== rules.length) throw new Error('invalid fixture');\n";
    }
  }
  return source;
}

// Script form for vm/ShadowRealm evaluation, which cannot evaluate ES modules.
// Returns an expression that evaluates to the namespace object.
export function asScript(esm: string): string {
  const names: string[] = [];
  const body = esm.replace(/^export (async )?function (\w+)/gm, (_, asyncKw, name) => {
    names.push(name);
    return `${asyncKw ?? ""}function ${name}`;
  });
  return `(() => {\n${body}\nreturn Object.freeze({${names.join(", ")}});\n})()`;
}
