// Prints notices for the production dependencies of workspace packages: `node scripts/npm-notices.mjs <package>...`.
// Fails on a license deny.toml does not allow. A package published without a license file needs its upstream text
// vendored at licenses/npm/<name>@<version>.txt.
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { allowedLicenses, satisfies } from "./spdx.mjs";

const licenseFile = /^(licen[cs]e|copying|notice|unlicense)([.-][\w-]+)?$/i;
const vendored = fileURLToPath(new URL("../licenses/npm/", import.meta.url));
const rule = "=".repeat(80);

const allowed = allowedLicenses();
const names = process.argv.slice(2);
const filters = names.flatMap((name) => ["--filter", name]);
const listed = JSON.parse(
  execFileSync("pnpm", [...filters, "licenses", "list", "--prod", "--json"], { encoding: "utf8" }),
);
const packages = Object.values(listed)
  .flat()
  .flatMap((pkg) => pkg.versions.map((version, index) => ({ ...pkg, version, path: pkg.paths[index] })))
  .sort((a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version));

const sections = [`Third-party notices for the production dependencies of ${names.join(", ")}.`];
for (const pkg of packages) {
  const id = `${pkg.name}@${pkg.version}`;
  if (!satisfies(pkg.license, allowed)) throw new Error(`${id} has a license deny.toml does not allow: ${pkg.license}`);
  const files = fs
    .readdirSync(pkg.path)
    .filter((file) => licenseFile.test(file))
    .sort();
  const texts = files.map((file) => fs.readFileSync(path.join(pkg.path, file), "utf8").trim());
  if (texts.length === 0) {
    const upstream = path.join(vendored, `${id}.txt`);
    if (!fs.existsSync(upstream))
      throw new Error(`${id} ships no license file; vendor its upstream text at ${upstream}`);
    texts.push(fs.readFileSync(upstream, "utf8").trim());
  }
  sections.push(`${rule}\n${id}: ${pkg.license}\n${rule}\n\n${texts.join("\n\n")}`);
}
process.stdout.write(`${sections.join("\n\n")}\n`);
