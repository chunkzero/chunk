import { expect, test } from "vitest";

import { formatBytes } from "./format.ts";

test("byte counts read in binary units", () => {
  expect(formatBytes(0n)).toBe("0 B");
  expect(formatBytes(1023n)).toBe("1023 B");
  expect(formatBytes(1536n)).toBe("1.5 KiB");
  expect(formatBytes(5n * 1024n ** 3n)).toBe("5.0 GiB");
});
