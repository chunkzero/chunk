import assert from "node:assert/strict";
import test from "node:test";

import { defineFunctions, defineSchema, mutation, v } from "../src/index.ts";

const reference = { path: "mail/send", kind: "action", arguments: v.object({ name: v.string() }), result: v.null() };
test("mutations schedule typed actions and require explicit retry acknowledgement", () => {
  const calls = [];
  const ctx = {
    caller: null,
    db: {},
    scheduler: {
      runAt: (...args) => {
        calls.push(args);
        return "job-one";
      },
      cancel: (id) => calls.push(["cancel", id]),
      retry: (...args) => calls.push(["retry", ...args]),
    },
  };
  for (const build of [mutation, defineFunctions(defineSchema({})).mutation]) {
    const work = build({
      args: {},
      returns: v.string(),
      handler: ({ scheduler }) => {
        assert.throws(() => scheduler.runAt(1, { ...reference, kind: "query" }, { name: "alex" }));
        assert.throws(() => scheduler.runAt(1, reference, { name: 0 }));
        assert.throws(() => scheduler.runAt(Number.MAX_SAFE_INTEGER + 1, reference, { name: "alex" }));
        assert.throws(() => scheduler.retry("job-one", 2, { acknowledgePossibleEffects: false }));
        const id = scheduler.runAt(1, reference, { name: "alex" });
        scheduler.cancel(id);
        scheduler.retry(id, 2, { acknowledgePossibleEffects: true });
        return id;
      },
    });
    assert.equal(work.handler(ctx, {}), "job-one");
  }
  assert.deepEqual(
    calls,
    Array(2)
      .fill([
        [1, "mail/send", { name: "alex" }],
        ["cancel", "job-one"],
        ["retry", "job-one", 2, true],
      ])
      .flat(),
  );
});
