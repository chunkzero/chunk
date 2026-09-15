import assert from "node:assert/strict";
import test from "node:test";

import { sessionMethod, v } from "../src/index.ts";
import { isSessionMethod } from "../src/sessions.ts";

test("authored method refs preserve identity and validate both wire directions", () => {
  const method = sessionMethod({
    app: "duels",
    session: "ranked",
    name: "forfeit",
    args: { player: v.player() },
    returns: v.object({ accepted: v.boolean(), reason: v.optional(v.string()) }),
  });
  assert.equal(isSessionMethod(method), true);
  assert.equal(isSessionMethod({ app: "duels", session: "ranked", name: "forfeit" }), false);
  assert.deepEqual(method.arguments.parse({ player: "alex" }), { player: "alex" });
  assert.throws(() => method.arguments.parse({ player: 42 }));
  assert.throws(() => method.arguments.parse({ player: "alex", authority: "admin" }));
  assert.deepEqual(method.result.parse({ accepted: false, reason: null }), { accepted: false });
  assert.throws(() => {
    method.app = "other";
  });
  assert.throws(() =>
    sessionMethod({ app: "../duels", session: "default", name: "forfeit", args: {}, returns: v.null() }),
  );
});
