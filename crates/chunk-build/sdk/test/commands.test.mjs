import assert from "node:assert/strict";
import test from "node:test";

import { command, commandArg, commandRoute, isCommand } from "../src/commands.ts";

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
});
