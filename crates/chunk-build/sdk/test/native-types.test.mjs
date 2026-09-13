import assert from "node:assert/strict";
import test from "node:test";

import { query, v } from "../src/index.ts";
import { v as schemaValidators } from "../src/schema.ts";

test("built-in schemas compose ordinary validators with the proxy's field contracts", () => {
  const cases = [
    [v.playerIdentity(), { uuid: v.string(), username: v.string() }, { uuid: "player-uuid", username: "Alex" }],
    [
      v.destination(),
      { key: v.string(), session_type: v.string(), machine_profile: v.string() },
      { key: "lobby", session_type: "lobby/default", machine_profile: "local" },
    ],
    [v.admissionResult(), { allow: v.boolean(), reason: v.optional(v.string()) }, { allow: true }],
    [
      v.serverStatus(),
      { motd: v.string(), online: v.integer(), max: v.integer() },
      { motd: "Server", online: 0, max: 16 },
    ],
  ];
  for (const [builtin, fields, value] of cases) {
    assert.deepEqual(builtin.schema, v.object(fields).schema);
    assert.deepEqual(builtin.parse(value), value);
    assert.throws(() => builtin.parse({}));
    assert.throws(() => builtin.parse({ ...value, unknown: true }));
    const options = { returns: builtin, handler: (_, args) => args };
    assert.deepEqual(query({ ...options, args: builtin }).contract, query({ ...options, args: fields }).contract);
  }
  assert.equal(schemaValidators.playerIdentity, v.playerIdentity);
  assert.deepEqual(v.admissionResult().parse({ allow: false, reason: "Maintenance" }), {
    allow: false,
    reason: "Maintenance",
  });
  assert.throws(() => v.serverStatus().parse({ motd: "Server", online: 0.5, max: 16 }));
  assert.throws(() => v.player().parse({ uuid: "player-uuid", username: "Alex" }));
});

test("destination composition adds fields without changing the built-in identity", () => {
  const args = v.playerIdentity().extend({ destination: v.destination() });
  const value = {
    uuid: "player-uuid",
    username: "Alex",
    destination: { key: "lobby", session_type: "lobby/default", machine_profile: "local" },
  };
  assert.deepEqual(args.parse(value), value);
  assert.throws(() => v.playerIdentity().parse(value));
  assert.throws(() => args.parse({ ...value, destination: { key: "lobby" } }));
});
