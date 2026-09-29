// Prints notices for the production dependencies of workspace packages: `node scripts/npm-notices.mjs <package>...`.
// Fails on a license outside the allowlist, which matches deny.toml.
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

const allowed = new Set([
  "0BSD",
  "Apache-2.0",
  "BSD-2-Clause",
  "BSD-3-Clause",
  "BSL-1.0",
  "CC0-1.0",
  "ISC",
  "MIT",
  "MIT-0",
  "MPL-2.0",
  "Unicode-3.0",
  "Unlicense",
  "Zlib",
]);
const licenseFile = /^(licen[cs]e|copying|notice)([.-][\w-]+)?$/i;
const rule = "=".repeat(80);
const spdx = new URL("../licenses/spdx/", import.meta.url);

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
const fallbacks = new Set();
for (const pkg of packages) {
  const alternatives = pkg.license.replace(/[()]/g, "").split(" OR ");
  const elected = alternatives
    .map((terms) => terms.split(" AND "))
    .find((terms) => terms.every((id) => allowed.has(id)));
  if (!elected) throw new Error(`${pkg.name}@${pkg.version} has a license that is not allowed: ${pkg.license}`);
  const files = fs
    .readdirSync(pkg.path)
    .filter((file) => licenseFile.test(file))
    .sort();
  const texts = files.map((file) => fs.readFileSync(path.join(pkg.path, file), "utf8").trim());
  if (texts.length === 0) {
    const { author, repository } = JSON.parse(fs.readFileSync(path.join(pkg.path, "package.json"), "utf8"));
    const by = typeof author === "string" ? author : author?.name;
    const source = typeof repository === "string" ? repository : repository?.url;
    texts.push(
      `The published package has no license file. It is licensed under ${pkg.license}` +
        `${by ? ` by ${by}` : ""}; the full text of each license is at the end of this file.` +
        `${source ? `\nSource: ${source}` : ""}`,
    );
    for (const id of elected) fallbacks.add(id);
  }
  sections.push(`${rule}\n${pkg.name} ${pkg.version}: ${pkg.license}\n${rule}\n\n${texts.join("\n\n")}`);
}
for (const id of [...fallbacks].sort()) {
  sections.push(`${rule}\n${id}\n${rule}\n\n${fs.readFileSync(new URL(`${id}.txt`, spdx), "utf8").trim()}`);
}
process.stdout.write(`${sections.join("\n\n")}\n`);
