import fs from "node:fs";

/** The license allowlist in deny.toml, shared by the Rust and npm checks. */
export function allowedLicenses() {
  const deny = fs.readFileSync(new URL("../deny.toml", import.meta.url), "utf8");
  const list = /^allow = \[([^\]]*)\]/m.exec(deny)?.[1];
  if (!list) throw new Error("deny.toml has no license allowlist");
  return new Set([...list.matchAll(/"([^"]+)"/g)].map((match) => match[1]));
}

/**
 * Whether the SPDX `expression` can be satisfied with `allowed` licenses: every AND operand must be allowed, and at
 * least one OR alternative. AND binds tighter than OR; `id WITH exception` must be allowed as a whole.
 */
export function satisfies(expression, allowed) {
  const tokens = expression.match(/\(|\)|[^\s()]+/g) ?? [];
  let position = 0;
  const peek = () => tokens[position]?.toUpperCase();
  const fail = () => {
    throw new Error(`Invalid SPDX expression: ${expression}`);
  };

  const either = () => {
    let result = both();
    while (peek() === "OR") {
      position++;
      result = both() || result;
    }
    return result;
  };
  const both = () => {
    let result = term();
    while (peek() === "AND") {
      position++;
      result = term() && result;
    }
    return result;
  };
  const term = () => {
    if (peek() === "(") {
      position++;
      const result = either();
      if (tokens[position++] !== ")") fail();
      return result;
    }
    let id = tokens[position++];
    if (id === undefined || ["AND", "OR", "WITH", ")"].includes(id.toUpperCase())) fail();
    if (peek() === "WITH") {
      const exception = tokens[position + 1];
      if (exception === undefined || exception === "(" || exception === ")") fail();
      id = `${id} WITH ${exception}`;
      position += 2;
    }
    return allowed.has(id);
  };

  const result = either();
  if (position !== tokens.length) fail();
  return result;
}
