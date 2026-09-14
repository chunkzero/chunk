import assert from "node:assert/strict";
import test from "node:test";

import { defineFunctions, defineSchema, defineTable, internalQuery, query, v } from "../src/index.ts";

test("context providers are ordered, awaited and isolated for each invocation", async () => {
  const calls = [];
  let release;
  const waiting = new Promise((resolve) => {
    release = resolve;
  });
  const enriched = query
    .withContext(async ({ caller }) => {
      if (caller.player === "alice") await waiting;
      calls.push(caller.player);
      return { player: { id: caller.player } };
    })
    .withContext(({ player }) => ({ greeting: `Hello, ${player.id}` }));
  const options = {
    args: { suffix: v.string() },
    returns: v.string(),
    handler: (ctx, args) => ctx.greeting + args.suffix,
  };
  const read = enriched(options);
  assert.deepEqual(read.contract, query(options).contract);
  const alice = read.handler({ caller: { player: "alice" }, db: {} }, { suffix: "!" });
  assert.equal(await read.handler({ caller: { player: "bob" }, db: {} }, { suffix: "?" }), "Hello, bob?");
  release();
  assert.equal(await alice, "Hello, alice!");
  assert.deepEqual(calls, ["bob", "alice"]);
  assert.equal(
    query({
      args: {},
      returns: v.boolean(),
      handler: (ctx) => "player" in ctx,
    }).handler({ caller: null, db: {} }, {}),
    false,
  );
});

test("context enrichment cannot overwrite existing fields or change caller identity", async () => {
  const context = { caller: { player: "alice" }, db: {} };
  const options = { args: {}, returns: v.null(), handler: () => assert.fail("handler must not run") };
  for (const extra of [{ caller: { player: "bob" } }, { db: {} }, [], null, new Date(0)]) {
    const read = query.withContext(() => extra)(options);
    await assert.rejects(read.handler(context, {}), /Context/);
  }
  for (const key of ["toString", "constructor"]) {
    const read = query.withContext(() => ({ [key]: "custom" }))({
      args: {},
      returns: v.string(),
      handler: (ctx) => ctx[key],
    });
    assert.equal(await read.handler(context, {}), "custom");
  }
  await assert.rejects(
    query
      .withContext(({ caller }) => {
        caller.player = "bob";
        return {};
      })(options)
      .handler(context, {}),
    TypeError,
  );
  await assert.rejects(
    query
      .withContext(() => ({ rank: "member" }))
      .withContext(() => ({ rank: "admin" }))(options)
      .handler(context, {}),
    /Context field already exists: rank/,
  );
  assert.deepEqual(context.caller, { player: "alice" });
});

test("schema-bound providers share the invocation reader or writer and may reject before the handler", async () => {
  const typed = defineFunctions(defineSchema({ profiles: defineTable({ rank: v.string() }) }));
  const writes = [];
  const context = {
    caller: { player: "alice" },
    db: { get: () => ({ rank: "member" }), put: (...args) => writes.push(args) },
  };
  const loadProfile = ({ db }) => ({ profile: db.get("profiles:alice") });
  const options = { args: {}, returns: v.string(), handler: ({ profile }) => profile.rank };
  const read = typed.query.withContext(loadProfile).withContext(({ db }) => {
    assert.equal("patch" in db, false);
    return {};
  })(options);
  assert.equal(await read.handler(context, {}), "member");
  const edit = typed.mutation.withContext(loadProfile)({
    args: {},
    returns: v.null(),
    handler: ({ db, profile }) => {
      db.patch(profile._id, { rank: "admin" });
      return null;
    },
  });
  await edit.handler(context, {});
  assert.deepEqual(writes, [["profiles", "profiles:alice", { rank: "admin" }]]);
  const denied = internalQuery.withContext(() => {
    throw new Error("Player required");
  })({
    args: {},
    returns: v.null(),
    handler: () => assert.fail("handler must not run"),
  });
  assert.equal(denied.contract.visibility, "internal");
  await assert.rejects(denied.handler({ caller: null, db: {} }, {}), /Player required/);
});
