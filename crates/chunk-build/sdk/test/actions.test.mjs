import assert from "node:assert/strict";
import test from "node:test";

import { action, internalAction, mutation, v } from "../src/index.ts";

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

test("actions forward named HTTP and secret capabilities without adding authority fields", async () => {
  const calls = [];
  const work = action({
    args: {},
    returns: v.string(),
    handler: async (ctx) => {
      const token = await ctx.secret("token");
      const result = await ctx.http("payments", { path: "status", headers: { authorization: token } });
      if (result.state !== "completed") return result.state;
      return result.body;
    },
  });
  const result = await work.handler(
    {
      caller: null,
      invocationId: "one",
      secret: async (name) => {
        calls.push(name);
        return "fixture-token";
      },
      http: async (binding, request) => {
        calls.push([binding, request]);
        return { state: "completed", effectId: "one/http/2", status: 200, headers: {}, body: "paid" };
      },
    },
    {},
  );
  assert.equal(result, "paid");
  assert.deepEqual(calls, ["token", ["payments", { path: "status", headers: { authorization: "fixture-token" } }]]);
});

test("actions move players through the platform, and mutations cannot", async () => {
  const player = "00000000-0000-0000-0000-000000000001";
  const destination = { key: "arena", session_type: "arena/default", machine_profile: "small" };
  const requests = [];
  const relocate = action({
    args: {},
    returns: v.string(),
    handler: async (ctx) => {
      const moved = await ctx.routing.move(player, destination);
      return moved.state === "accepted" ? moved.operationId : moved.reason;
    },
  });
  const run = (outcome) =>
    relocate.handler(
      {
        caller: null,
        invocationId: "one",
        platform: async (request) => {
          requests.push(request);
          return outcome;
        },
      },
      {},
    );
  assert.equal(await run({ state: "accepted", operationId: "action/one/platform/1" }), "action/one/platform/1");
  assert.equal(await run({ state: "refused", reason: "full" }), "full");
  await assert.rejects(run({ state: "refused", reason: "busy" }));
  assert.deepEqual(requests[0], { kind: "move", player, destination });

  let context;
  const record = mutation({
    args: {},
    returns: v.null(),
    handler: (ctx) => {
      context = ctx;
      return null;
    },
  });
  record.handler({ caller: null, db: {}, scheduler: {} }, {});
  assert.equal("routing" in context, false);
});
