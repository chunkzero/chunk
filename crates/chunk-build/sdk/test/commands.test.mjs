import assert from "node:assert/strict";
import test from "node:test";

import { command, commandArg, commandRoute, invokeCommand, isCommand } from "../src/commands.ts";
import { sessionMethod, v } from "../src/index.ts";

test("command routes retain named parser order and independently typed handlers", async () => {
  let observed;
  const party = command("party", {
    aliases: ["p"],
    routes: [
      commandRoute(["invite"], {
        args: { player: commandArg.word({ suggestions: ["Alex"] }), count: commandArg.integer({ min: 1, max: 8 }) },
        handler: (_, args) => {
          observed = args;
        },
      }),
      commandRoute(["leave"], { handler: () => {} }),
    ],
  });
  assert.equal(isCommand(party), true);
  assert.equal(
    isCommand(() => {}),
    false,
  );
  assert.deepEqual(party.contract.routes[0], {
    literals: ["invite"],
    arguments: [
      { name: "player", parser: "word", suggestions: ["Alex"] },
      { name: "count", parser: "integer", min: 1, max: 8 },
    ],
  });
  await party.routes[0].handler({}, { player: "Alex", count: 2 });
  assert.deepEqual(observed, { player: "Alex", count: 2 });
  assert.throws(() => party.contract.aliases.push("other"));
});

test("command grammar rejects ambiguous routes and unsupported parser shapes", () => {
  const handler = () => {};
  assert.throws(() => command("party", { aliases: ["party"], handler }));
  assert.throws(() => command("Party", { handler }));
  assert.throws(() => command("party", { routes: [commandRoute([], { handler }), commandRoute([], { handler })] }));
  assert.throws(() => command("party", { args: { message: commandArg.greedy(), after: commandArg.word() }, handler }));
  assert.throws(() => command("party", { args: { player: commandArg.word(), Player: commandArg.word() }, handler }));
  assert.throws(() => commandArg.integer({ max: 2 ** 31 }));
  assert.throws(() => commandArg.integer({ min: 3, max: 2 }));
  assert.throws(() => commandArg.word({ suggestions: ["same", "same"] }));
  assert.throws(() => command("party", { handler: null }));
  assert.throws(() => command("party", { args: { message: { parser: "minecraft:message" } }, handler }));
});

test("compiled command adapters preserve authenticated context and route identity", async () => {
  const captured = [];
  const descriptor = command("party", {
    routes: [
      commandRoute(["invite"], {
        args: { target: commandArg.word() },
        handler: async (ctx, args) => {
          assert.equal(Object.isFrozen(ctx.player), true);
          assert.deepEqual(ctx.caller, { player: "trusted" });
          assert.equal(typeof ctx.routing.enter, "function");
          assert.equal("move" in ctx.routing, false);
          await ctx.runMutation(
            {
              path: "shared/invite",
              kind: "mutation",
              arguments: v.object({ target: v.string(), from: v.string() }),
              result: v.null(),
            },
            { target: args.target, from: ctx.player.uuid },
          );
        },
      }),
    ],
  });
  const raw = {
    caller: { player: "trusted" },
    runMutation: async (path, args) => {
      captured.push([path, args]);
      return null;
    },
  };
  const payload = { route: 0, arguments: { target: "Alex" }, player: { uuid: "id", username: "Other" } };
  assert.equal(await invokeCommand(descriptor, raw, payload), null);
  assert.deepEqual(captured, [["shared/invite", { target: "Alex", from: "id" }]]);
  await assert.rejects(invokeCommand(descriptor, raw, { ...payload, route: 1 }), /Unknown command route/);
});

test("literal and argument children need distinct names at the same route prefix", () => {
  const handler = () => {};
  const route = commandRoute(["admin"], { args: { list: commandArg.word() }, handler });
  assert.throws(
    () => command("travel", { routes: [route, commandRoute(["admin", "list"], { handler })] }),
    /argument name conflicts/,
  );
  assert.doesNotThrow(() => command("travel", { routes: [route, commandRoute(["public", "list"], { handler })] }));
});

test("command effects keep captured targets and validate typed method calls", async () => {
  const method = sessionMethod({
    app: "lobby",
    session: "default",
    name: "greet",
    args: { name: v.string() },
    returns: v.string(),
  });
  const requests = [];
  const descriptor = command("greet", {
    handler: async (ctx) => {
      assert.equal(ctx.invocationId, "command/one");
      const receipt = await ctx.player.message("Hello");
      assert.deepEqual(receipt, { state: "accepted", operationId: "effect/1" });
      assert.equal(await ctx.session.call(method, { name: "Alex" }), "Hello Alex");
      await ctx.session.send(method, { name: "Other" });
      await ctx.routing.enter({ key: "main", session_type: "lobby/default", machine_profile: "small" });
      await assert.rejects(ctx.session.call(method, { name: 1 }));
      assert.throws(() => ctx.player.title("x".repeat(4097)));
      assert.equal(Object.isFrozen(ctx.session), true);
    },
  });
  await invokeCommand(
    descriptor,
    {
      caller: { kind: "proxy" },
      invocationId: "command/one",
      platform: async (request) => {
        requests.push(request);
        return request.kind === "session_call"
          ? "Hello Alex"
          : { state: "accepted", operationId: `effect/${requests.length}` };
      },
    },
    { route: 0, arguments: {}, player: { uuid: "trusted-id", username: "Alex" } },
  );
  assert.deepEqual(requests[1], {
    kind: "session_call",
    method: { app: "lobby", session: "default", name: "greet" },
    arguments: { name: "Alex" },
  });
  assert.equal(requests.length, 4);
  assert.equal(
    requests.every((request) => !("player" in request) && !("target" in request)),
    true,
  );
});
