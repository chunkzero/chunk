import assert from "node:assert/strict";
import test from "node:test";

import { defineFunctions, defineSchema, defineTable, unset, v } from "../src/index.ts";

const schema = defineSchema({
  profiles: defineTable({ wins: v.integer(), note: v.optional(v.union(v.null(), v.string())) }).index("by_wins", [
    "wins",
  ]),
});
const functions = defineFunctions(schema);

test("document arrays can be edited locally while metadata stays readonly", () => {
  const schema = defineSchema({ profiles: defineTable({ tags: v.array(v.string()) }) });
  let stored = { tags: ["old"] };
  const mutate = defineFunctions(schema).mutation({
    args: {},
    returns: v.null(),
    handler: ({ db }) => {
      const doc = db.get("profiles:p1");
      doc.tags.push("new");
      assert.deepEqual(stored.tags, ["old"]);
      assert.throws(() => {
        doc._id = "profiles:p2";
      });
      db.patch(doc._id, { tags: doc.tags });
      assert.deepEqual(stored.tags, ["old", "new"]);
      return null;
    },
  });
  mutate.handler(
    {
      caller: null,
      db: {
        get: () => structuredClone(stored),
        put: (_table, _id, value) => {
          stored = structuredClone(value);
        },
      },
    },
    {},
  );
});

test("typed selections omit open bounds and use bounded terminal reads", () => {
  const requests = [];
  const read = functions.query({
    args: {},
    returns: v.null(),
    handler: ({ db }) => {
      const query = db.query("profiles");
      assert.equal(query.withIndex("by_wins").first(), null);
      assert.equal(query.withIndex("by_wins", (q) => q.eq("wins", 2)).unique(), null);
      query.withIndex("by_wins", (q) => q.gte("wins", 2)).collect(10);
      query.withIndex("by_wins", (q) => q.lt("wins", 4)).collect(10);
      query.withIndex("by_wins", (q) => q.gte("wins", 2).lt("wins", 4)).collect(10);
      for (const limit of [0, 1025, 1.5]) assert.throws(() => query.withIndex("by_wins").collect(limit));
      return null;
    },
  });
  read.handler(
    {
      caller: null,
      db: {
        scanIndex(request) {
          requests.push(request);
          return [];
        },
      },
    },
    {},
  );
  assert.deepEqual(requests, [
    { table: "profiles", index: "by_wins", prefix: [], limit: 1 },
    { table: "profiles", index: "by_wins", prefix: [2], limit: 2 },
    { table: "profiles", index: "by_wins", prefix: [], start: 2, limit: 10 },
    { table: "profiles", index: "by_wins", prefix: [], end: 4, limit: 10 },
    { table: "profiles", index: "by_wins", prefix: [], start: 2, end: 4, limit: 10 },
  ]);
});

test("typed patches distinguish null from unset and queries expose no writes", () => {
  let stored = { wins: 1, note: "old" };
  const raw = {
    get: () => structuredClone(stored),
    put(table, id, value) {
      assert.equal(table, "profiles");
      assert.equal(id, "profiles:p1");
      stored = structuredClone(value);
    },
    delete: () => assert.fail("query reached raw delete"),
  };
  const mutate = functions.mutation({
    args: {},
    returns: v.null(),
    handler: ({ db }) => {
      db.patch("profiles:p1", { note: null });
      assert.deepEqual(stored, { wins: 1, note: null });
      db.patch("profiles:p1", { note: unset });
      assert.deepEqual(stored, { wins: 1 });
      assert.throws(() => db.patch("profiles:p1", { note: undefined }));
      assert.throws(() => db.patch("profiles:p1", { wins: unset }));
      assert.throws(() => db.get("matches:p1"));
      assert.throws(() => db.get("profiles:"));
      return null;
    },
  });
  mutate.handler({ caller: null, db: raw }, {});
  const read = functions.query({
    args: {},
    returns: v.null(),
    handler: ({ db }) => {
      for (const name of ["insert", "patch", "delete", "put"]) assert.equal(name in db, false);
      assert.throws(() => db.delete("profiles:p1"));
      return null;
    },
  });
  read.handler({ caller: null, db: raw }, {});
});
