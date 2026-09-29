import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { test } from "node:test";

test("dashboard notices carry credits for code copied into its dependencies and bundle", () => {
  const notices = execFileSync("node", ["scripts/npm-notices.mjs", "@chunkzero/dashboard"], {
    cwd: new URL("..", import.meta.url),
    encoding: "utf8",
  });
  for (const attribution of [
    // @radix-ui/primitive's getActiveElement comes from AriaKit.
    "MIT License, Copyright (c) AriaKit.",
    "Copyright (c) Diego Haz",
    // Rolldown's runtime helpers, credited to esbuild.
    "Copyright (c) 2020 Evan Wallace",
    // The dashboard's own components adapted from shadcn/ui.
    "Copyright (c) 2023 shadcn",
  ]) {
    assert.ok(notices.includes(attribution), attribution);
  }
});
