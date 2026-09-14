import assert from "node:assert/strict";
import test from "node:test";

import { defineFunctions, defineSchema, internalMutation, internalQuery, mutation, query, v } from "../src/index.ts";

test("object extension snapshots fields, replaces exact names and preserves optionality", () => {
  const fields = { name: v.string(), note: v.optional(v.string()) };
  const original = v.object(fields);
  fields.name = v.integer();
  const extended = original.extend({ note: v.integer(), extra: v.optional(v.boolean()) });
  assert.deepEqual(original.parse({ name: "Alex" }), { name: "Alex" });
  assert.deepEqual(extended.parse({ name: "Alex", note: 1 }), { name: "Alex", note: 1 });
  assert.throws(() => extended.parse({ name: "Alex" }));
  assert.throws(() => original.parse({ name: "Alex", note: 1 }));
  assert.throws(() => original.extend({ Name: v.string() }));
  assert.deepEqual(extended.extend({ note: v.optional(v.integer()) }).parse({ name: "Alex" }), { name: "Alex" });
  assert.deepEqual(v.object({ nested: extended }).parse({ nested: { name: "Alex", note: 1 } }), {
    nested: { name: "Alex", note: 1 },
  });
  assert.throws(() => {
    extended.schema.fields.name.optional = true;
  });
});

test("all function builders normalize object arguments to the existing contract", () => {
  const typed = defineFunctions(defineSchema({}));
  const fields = { parse: v.string(), schema: v.optional(v.integer()) };
  const args = v.object(fields);
  for (const build of [query, mutation, internalQuery, internalMutation, ...Object.values(typed)]) {
    const options = { returns: v.string(), handler: (_, args) => args.parse };
    const shorthand = build({ ...options, args: fields });
    const reusable = build({ ...options, args });
    assert.deepEqual(reusable.contract, shorthand.contract);
    for (const invalid of [v.string(), v.array(args), v.nullable(args), v.optional(args), v.union({ value: args })]) {
      assert.throws(() => build({ ...options, args: invalid }), /Function arguments must be an object/);
    }
  }
});
