import assert from "node:assert/strict";
import test from "node:test";

import { action, internalAction, mutation, query, v } from "../src/index.ts";

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

test("actions read env and fetch through the raw capability, failing on refused or uncertain effects", async () => {
  const requests = [];
  const outcomes = [
    { state: "completed", url: "https://example.com/paid", status: 200, headers: { a: "b" }, body: '{"paid":true}' },
    { state: "rejected", reason: "HTTP destination address refused" },
  ];
  const work = action({
    args: {},
    returns: v.string(),
    handler: async (ctx) => {
      const response = await ctx.fetch(new URL("https://example.com/status"), {
        method: "POST",
        headers: { authorization: ctx.env.TOKEN },
      });
      assert.deepEqual([response.url, response.ok, response.headers], ["https://example.com/paid", true, { a: "b" }]);
      assert.deepEqual(await response.json(), { paid: true });
      await assert.rejects(ctx.fetch("http://10.0.0.1/"), /refused/);
      return ctx.env.GREETING;
    },
  });
  const raw = {
    caller: null,
    invocationId: "one",
    env: { GREETING: "hello", TOKEN: "fixture-token" },
    fetch: async (request) => {
      requests.push(request);
      return outcomes.shift();
    },
  };
  assert.equal(await work.handler(raw, {}), "hello");
  assert.deepEqual(requests, [
    { method: "POST", headers: { authorization: "fixture-token" }, url: "https://example.com/status" },
    { url: "http://10.0.0.1/" },
  ]);
});

test("actions move players through the platform, and queries and mutations cannot", async () => {
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

  for (const define of [query, mutation]) {
    let context;
    const definition = define({
      args: {},
      returns: v.null(),
      handler: (ctx) => {
        context = ctx;
        return null;
      },
    });
    definition.handler({ caller: null, db: {}, scheduler: {} }, {});
    assert.equal("routing" in context, false);
  }
});
