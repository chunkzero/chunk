import assert from "node:assert/strict";
import test from "node:test";

import { action, internalAction, v } from "../src/index.ts";

test("actions expose scoped typed calls, preserving caller and invocation identity", async () => {
  const reference = {
    path: "profiles/read",
    kind: "query",
    arguments: v.object({ count: v.integer() }),
    result: v.string(),
  };
  const calls = [];
  const definition = action.withContext(({ caller }) => ({ player: caller.player }))({
    args: {},
    returns: v.string(),
    handler: async (ctx) => {
      assert.equal("db" in ctx, false);
      assert.equal(ctx.invocationId, "action-1");
      await ctx.sleep(10);
      return ctx.runQuery(reference, { count: 2 });
    },
  });
  assert.equal(definition.contract.kind, "action");
  const result = await definition.handler(
    {
      caller: { player: "alice" },
      invocationId: "action-1",
      sleep: async (milliseconds) => calls.push(milliseconds),
      runQuery: async (path, args) => {
        calls.push([path, args]);
        return "alice";
      },
    },
    {},
  );
  assert.equal(result, "alice");
  assert.deepEqual(calls, [10, ["profiles/read", { count: 2 }]]);
  assert.equal(internalAction({ args: {}, returns: v.null(), handler: () => null }).contract.visibility, "internal");
});

test("action references validate kind, arguments and results before application consumption", async () => {
  const reference = {
    path: "profiles/read",
    kind: "query",
    arguments: v.object({ count: v.integer() }),
    result: v.string(),
  };
  const invoke = (ref, args, result) =>
    action({ args: {}, returns: v.string(), handler: (ctx) => ctx.runQuery(ref, args) }).handler(
      { caller: null, invocationId: "one", runQuery: async () => result },
      {},
    );
  await assert.rejects(invoke({ ...reference, kind: "mutation" }, { count: 1 }, "okay"), /kind/);
  await assert.rejects(invoke(reference, { count: "wrong" }, "okay"));
  await assert.rejects(invoke(reference, { count: 1 }, 12));
});
