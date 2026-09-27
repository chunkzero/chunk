import assert from "node:assert/strict";
import test from "node:test";

import { invokeHook, isHook } from "../src/hooks.ts";
import { createHook, query, v } from "../src/index.ts";

test("named hooks have immutable metadata distinct from function and helper exports", () => {
  const hook = createHook("player.login", () => ({ allow: true }), { order: 10 });
  assert.deepEqual(hook.contract, { event: "player.login", order: 10 });
  assert.equal(isHook(hook), true);
  assert.equal(
    isHook(() => hook),
    false,
  );
  assert.equal(isHook(query({ args: {}, returns: v.null(), handler: () => null })), false);
  assert.throws(() => {
    hook.contract.order = 20;
  });
  for (const options of [{ order: 0.5 }, { order: 2147483648 }, { followPlayer: true }, { unknown: true }]) {
    assert.throws(() => createHook("player.login", () => ({ allow: true }), options));
  }
  assert.throws(() => createHook("server.ping", () => null, { order: 1 }));
  assert.throws(() => createHook("unknown", () => null));
});

test("the compiler adapter preserves trusted capabilities and validates decision results", async () => {
  const calls = [];
  const raw = {
    caller: { kind: "gateway", player: "trusted" },
    runQuery: async (path, args) => {
      calls.push([path, args]);
      return true;
    },
    runMutation: async () => null,
  };
  const payload = {
    eventId: "event",
    domain: "",
    player: { uuid: "uuid", username: "Alex" },
    destination: null,
    caller: "spoofed",
  };
  const hook = createHook("player.login", async (ctx) => {
    assert.deepEqual(ctx.caller, { kind: "gateway", player: "trusted" });
    assert.equal(Object.isFrozen(ctx.player), true);
    const allow = await ctx.runQuery({ path: "shared/allowed" }, {});
    return { allow };
  });
  assert.deepEqual(await invokeHook(hook, raw, payload), { allow: true });
  assert.deepEqual(calls, [["shared/allowed", {}]]);
  const ping = createHook("server.ping", (ctx) => {
    assert.equal(ctx.runMutation, undefined);
    return { motd: "Hello", online: 0, max: 16 };
  });
  assert.equal((await invokeHook(ping, raw, { eventId: "ping", domain: "" })).motd, "Hello");
  await assert.rejects(
    invokeHook(
      createHook("player.login", () => null),
      raw,
      payload,
    ),
  );
  assert.equal(
    await invokeHook(
      createHook("domain.enter", () => undefined),
      raw,
      payload,
    ),
    null,
  );
});
