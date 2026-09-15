import assert from "node:assert/strict";
import test from "node:test";

import { isDestination } from "../src/destinations.ts";
import { defineDestination } from "../src/index.ts";

const target = { key: "main", session_type: "lobby/default", machine_profile: "local" };

test("destinations freeze a plain routing target and bounded immutable pool policy", () => {
  const pool = defineDestination(target);
  assert.equal(isDestination(pool), true);
  assert.equal(isDestination(pool.destination), false);
  assert.deepEqual(pool.destination, target);
  assert.deepEqual(pool.contract, { destination: target, overflow: "replicate", empty_timeout_seconds: 60 });
  assert.throws(() => {
    pool.destination.key = "other";
  });
  const single = defineDestination({ ...target, overflow: "reject", emptyTimeoutSeconds: 1 });
  assert.equal(single.contract.overflow, "reject");
  assert.equal(single.contract.empty_timeout_seconds, 1);
});

test("groups, mutable parameters, malformed keys and unbounded policies are rejected", () => {
  for (const extra of [
    { group: ["alice", "bob"] },
    { roster: [] },
    { parameters: { map: "new" } },
    { key: "" },
    { key: "é".repeat(65) },
    { key: "line\nfeed" },
    { session_type: "lobby" },
    { session_type: "lobby/../arena" },
    { machine_profile: "" },
    { overflow: "replace" },
    { emptyTimeoutSeconds: 0 },
    { emptyTimeoutSeconds: 86401 },
    { emptyTimeoutSeconds: 0.5 },
  ])
    assert.throws(() => defineDestination({ ...target, ...extra }));
});
