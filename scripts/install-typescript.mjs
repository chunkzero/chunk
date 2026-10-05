import fs from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import getExePath from "../node_modules/typescript/lib/getExePath.js";

const version = "7.0.2";
const destination = path.resolve(process.argv[2] ?? "target/debug");
// The installed compiler matches this Node.js build's platform, so a cross-platform SDK needs a matching Node.js.
const platform = `${process.platform === "win32" ? "windows" : process.platform}-${process.arch}`;
if (process.argv[3] && process.argv[3] !== platform) {
  throw new Error(`Expected a ${process.argv[3]} Node.js to install its TypeScript compiler, got ${platform}`);
}
const compiler = getExePath();
const pkg = JSON.parse(await fs.readFile(new URL("../node_modules/typescript/package.json", import.meta.url), "utf8"));
if (pkg.version !== version) throw new Error(`Expected TypeScript ${version}`);
const root = path.join(destination, "toolchain/typescript");
await fs.mkdir(root, { recursive: true });
const staging = await fs.mkdtemp(path.join(root, ".install-"));
try {
  await fs.cp(path.dirname(compiler), staging, { recursive: true });
  for (const name of ["LICENSE", "NOTICE.txt"]) {
    await fs.copyFile(
      fileURLToPath(new URL(`../node_modules/typescript/${name}`, import.meta.url)),
      path.join(staging, name),
    );
  }
  const target = path.join(root, version);
  try {
    await fs.rename(staging, target);
  } catch (error) {
    if (!["ENOTEMPTY", "EEXIST"].includes(error.code)) throw error;
  }
  console.log(`TypeScript ${version}: ${target}`);
} finally {
  await fs.rm(staging, { recursive: true, force: true });
}
