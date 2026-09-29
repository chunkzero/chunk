import assert from "node:assert/strict";
import { test } from "node:test";

import { allowedLicenses, satisfies } from "./spdx.mjs";

const allowed = new Set(["MIT", "Apache-2.0", "BSD-3-Clause", "Apache-2.0 WITH LLVM-exception"]);

test("accepts allowed licenses and expressions", () => {
  for (const expression of [
    "MIT",
    "MIT OR Apache-2.0",
    "(Apache-2.0 AND BSD-3-Clause)",
    "GPL-3.0-only OR MIT",
    "(MIT OR GPL-3.0-only) AND Apache-2.0",
    "MIT AND BSD-3-Clause OR GPL-3.0-only",
    "Apache-2.0 WITH LLVM-exception",
  ]) {
    assert.ok(satisfies(expression, allowed), expression);
  }
});

test("rejects expressions that require a license outside the allowlist", () => {
  for (const expression of [
    "GPL-3.0-only",
    "(MIT OR Apache-2.0) AND GPL-3.0-only",
    "GPL-3.0-only AND (MIT OR Apache-2.0)",
    "MIT AND (GPL-3.0-only OR AGPL-3.0-only)",
    "GPL-2.0-only WITH Classpath-exception-2.0",
  ]) {
    assert.equal(satisfies(expression, allowed), false, expression);
  }
});

test("rejects malformed expressions", () => {
  for (const expression of ["", "(MIT", "MIT OR", "MIT Apache-2.0", "MIT WITH", "SEE LICENSE IN LICENSE.md"]) {
    assert.throws(() => satisfies(expression, allowed), expression);
  }
});

test("reads the allowlist from deny.toml", () => {
  const licenses = allowedLicenses();
  assert.ok(licenses.has("MIT") && licenses.has("CDLA-Permissive-2.0"));
  assert.ok(licenses.has("Apache-2.0 WITH LLVM-exception"));
  assert.ok(!licenses.has("GPL-3.0-only"));
});
