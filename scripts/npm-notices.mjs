// Prints notices for the production dependencies of workspace packages: `node scripts/npm-notices.mjs <package>...`.
// Fails on a license deny.toml does not allow. Each package's license files are followed by the license notices and
// credits to other projects in the comments of its shipped files, including source maps' sources. A package published
// without a license file needs its upstream text vendored at licenses/npm/<name>@<version>.txt, and a credited project
// whose license the comment names without its text needs an entry in `credited`. Vendored texts are added by hand.
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { allowedLicenses, satisfies } from "./spdx.mjs";

// Projects credited in shipped comments that leave out their license text, by a marker in the comment: the text
// vendored at licenses/npm/credited/<name>.txt, or null where the credited source publishes no license.
const credited = {
  "ariakit/ariakit": "ariakit",
  "stackblitz/alien-signals": "alien-signals",
  "lukeed/clsx": "clsx",
  "`fast-deep-equal`": "fast-deep-equal",
  "zertosh/htmlescape": "htmlescape",
  "jonschlinkert/is-plain-object": "is-plain-object",
  "reach/observe-rect": "observe-rect",
  "substack/point-in-polygon": "point-in-polygon",
  "`qss`": "qss",
  "developerway.com": null,
};
// Code in a workspace package itself that is adapted from other projects, with the credited text that covers it.
const adapted = {
  "@chunkzero/dashboard": [
    ["src/components/ui adapts components from shadcn/ui (https://github.com/shadcn-ui/ui).", "shadcn-ui"],
  ],
};
// Packages whose code the build adds to a workspace package's output besides its production dependencies.
const injected = {
  "@chunkzero/dashboard": ["rolldown"], // Rolldown's runtime helpers in the bundle.
};

const licenseFile = /^((third[-_]party[-_])?(licen[cs]es?|notices?)|copying|unlicense)([.-][\w-]+)?$/i;
const shippedFile = /\.(c?js|mjs|d\.[cm]?ts|css|map)$/;
const copyright =
  /\b(Copyright|COPYRIGHT)(\s*(\(c\)|\(C\)|©|ⓒ))*\s+(?!(Notice|NOTICE|HOLDER|OWNER))[\dA-Z]|©|SPDX-FileCopyrightText:/;
const grant = new RegExp(
  [
    "permission is hereby granted|redistribution and use|permission to use, copy, modify|licensed under",
    "under the terms of|governed by|SPDX-License-Identifier:\\s*\\w|@license\\b",
    "\\b(MIT|BSD|ISC|Apache|zlib|Boost|MPL)\\b[\\w\\s.-]{0,20}licen[cs]e|licen[cs]e\\W{1,3}(MIT|BSD|ISC|Apache|zlib|Boost|MPL)\\b",
  ].join("|"),
  "i",
);
const complete = (text) => copyright.test(text) && grant.test(text);
// License terms written out rather than named.
const terms = /permission is hereby granted|redistribution and use|permission to use, copy, modify/i;
// "Copied from https://…", "adapted from Alien Signals", "based on https://…", "fork of `fast-deep-equal`" and the like,
// with the phrase matched in any case but the credited name in its own. "Derived from" and "based on" are common in
// prose, so they count only with a URL.
const creditPhrase =
  /\b(?:(?:copied|adapted|ported|taken|borrowed|lifted|forked|vendored|extracted)\s+from:?|(derived from:?|based on:?)|(?:fork|port|reimplementation)\s+of(?:\s+the)?)\s+(\S+)/gi;
const credits = (text) =>
  [...text.matchAll(creditPhrase)].some(([, loose, name]) =>
    (loose ? /^https?:\/\// : /^(https?:\/\/|`|[A-Z])/).test(name),
  );
const vendored = fileURLToPath(new URL("../licenses/npm/", import.meta.url));
const rule = "=".repeat(80);

/**
 * The text of each block comment and each run of line comments, wherever they start in a line, with whether only
 * whitespace separates it from the one before.
 */
function comments(source) {
  const found = [];
  let block = [];
  let kind = null;
  let adjacent = false;
  let code = true;
  const flush = () => {
    if (block.length) found.push({ lines: block, adjacent });
    block = [];
    kind = null;
  };
  const start = () => {
    if (block.length) return;
    adjacent = !code;
    code = false;
  };
  for (const line of source.split(/\r?\n/)) {
    let rest = line;
    let leading = true;
    if (kind === "/*") {
      const end = line.indexOf("*/");
      block.push((end < 0 ? line : line.slice(0, end)).replace(/^\s*\*(?!\/) ?/, ""));
      if (end < 0) continue;
      flush();
      rest = line.slice(end + 2);
      leading = false;
    }
    let match;
    while ((match = /\/\*|(?<![:/\\])\/\/(\/(?!\/)|!)?/.exec(rest))) {
      const after = rest.slice(match.index + match[0].length);
      if (rest.slice(0, match.index).trim()) code = true;
      if (match[0] !== "/*") {
        if (!(leading && kind === match[0] && !rest.slice(0, match.index).trim())) flush();
        start();
        block.push(after.replace(/^ /, ""));
        kind = match[0];
        break;
      }
      flush();
      start();
      const end = after.indexOf("*/");
      block.push((end < 0 ? after : after.slice(0, end)).replace(/^[*!]+/, "").trim());
      if (end < 0) {
        kind = "/*";
        break;
      }
      flush();
      rest = after.slice(end + 2);
      leading = false;
    }
    if (!match) {
      if (rest.trim()) code = true;
      flush();
    }
  }
  if (kind !== "/*") flush();
  return found.map(({ lines, adjacent }) => ({ text: dedent(lines).trim(), adjacent }));
}

function dedent(lines) {
  const indents = lines.filter((line) => line.trim()).map((line) => /^[ \t]*/.exec(line)[0].length);
  const indent = Math.min(...indents, Infinity);
  return lines.map((line) => line.slice(Number.isFinite(indent) ? indent : 0).trimEnd()).join("\n");
}

/** Normalized copyright holders named in a text, such as "meta platforms inc and affiliates". */
function holders(text) {
  const found = new Set();
  for (const line of text.split("\n")) {
    const match = copyright.exec(line);
    if (!match) continue;
    const statement = line
      .slice(match.index)
      .split(/\.\s+(?=[A-Z])/)[0]
      .replace(/(spdx-file)?copyright(text)?|\(c\)|all rights reserved|\d{4}|[^\p{L}\p{N}_\s]/giu, " ");
    const words = statement.toLowerCase().split(/\s+/).filter(Boolean);
    const holder = (words[0] === "the" ? words.slice(1) : words).join(" ");
    if (holder) found.add(holder);
  }
  return found;
}

function* files(directory, root = directory) {
  for (const entry of fs.readdirSync(directory, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
    const file = path.join(directory, entry.name);
    if (entry.isDirectory()) {
      if (entry.name !== "node_modules") yield* files(file, root);
    } else yield path.relative(root, file);
  }
}

/** The notices and credits in the comments of a package's shipped files that its license files do not already carry. */
function fileNotices(pkg, own) {
  const found = new Map();
  for (const file of files(pkg.path)) {
    if (!shippedFile.test(file)) continue;
    let sources = [[file, fs.readFileSync(path.join(pkg.path, file), "utf8")]];
    if (file.endsWith(".map")) {
      const map = JSON.parse(sources[0][1]);
      sources = (map.sourcesContent ?? []).map((text, index) => [`${file} (${map.sources[index]})`, text ?? ""]);
    }
    for (const [label, source] of sources) {
      const blocks = comments(source);
      for (const [index, block] of blocks.entries()) {
        // A copyright line and its license terms can be in adjacent comments.
        const previous = blocks[index - 1]?.text;
        const paired = `${previous}\n\n${block.text}`;
        const text =
          !complete(block.text) && block.adjacent && !complete(previous) && complete(paired) ? paired : block.text;
        const named = holders(text);
        const notice = complete(text) && [...named].some((holder) => !own.has(holder));
        if (!notice && !credits(text)) continue;
        const key = text.toLowerCase().split(/\s+/).join(" ");
        if (!found.has(key)) found.set(key, { text, labels: [] });
        found.get(key).labels.push(label);
      }
    }
  }
  return [...found.values()];
}

/** The names of the credited texts the notices need, collecting those that name a project without one in `uncredited`. */
function creditedNames(id, notices) {
  const found = new Set();
  for (const { text } of notices) {
    const markers = Object.keys(credited).filter((marker) => text.toLowerCase().includes(marker.toLowerCase()));
    if (markers.length === 0 && !terms.test(text)) uncredited.push(`${id}:\n${text}`);
    for (const marker of markers) if (credited[marker] !== null) found.add(credited[marker]);
  }
  return [...found];
}

const subsection = (title, parts) => `${"-".repeat(80)}\n${title}\n${"-".repeat(80)}\n\n${parts.join("\n\n")}`;
const creditedText = (name) => fs.readFileSync(path.join(vendored, "credited", `${name}.txt`), "utf8").trim();

function authorName(author) {
  return typeof author === "string" ? author.replace(/<.*?>|\(.*?\)/g, "") : (author?.name ?? "");
}

const allowed = allowedLicenses();
const names = process.argv.slice(2);
const filters = names.flatMap((name) => ["--filter", name]);
const list = (...args) =>
  Object.values(
    JSON.parse(execFileSync("pnpm", [...filters, "licenses", "list", "--json", ...args], { encoding: "utf8" })),
  )
    .flat()
    .flatMap((pkg) => pkg.versions.map((version, index) => ({ ...pkg, version, path: pkg.paths[index] })));
const buildTools = names.flatMap((name) => injected[name] ?? []);
const packages = [
  ...list("--prod"),
  ...(buildTools.length > 0 ? list() : [])
    .filter((pkg) => buildTools.includes(pkg.name))
    .map((pkg) => ({ ...pkg, injected: true })),
].sort((a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version));

const uncredited = [];
const sections = [`Third-party notices for the production dependencies of ${names.join(", ")}.`];
for (const name of names) {
  const parts = (adapted[name] ?? []).flatMap(([statement, credit]) => [statement, creditedText(credit)]);
  if (parts.length > 0)
    sections.push(`${rule}\nCode in ${name} adapted from other projects\n${rule}\n\n${parts.join("\n\n")}`);
}
for (const pkg of packages) {
  const id = `${pkg.name}@${pkg.version}`;
  if (!satisfies(pkg.license, allowed)) throw new Error(`${id} has a license deny.toml does not allow: ${pkg.license}`);
  const texts = [...files(pkg.path)]
    .filter((file) => licenseFile.test(path.basename(file)) && !/\.([cm]?[jt]sx?|json|map|css)$/.test(file))
    .sort((a, b) => a.split("/").length - b.split("/").length || a.localeCompare(b))
    .map((file) => fs.readFileSync(path.join(pkg.path, file), "utf8").trim());
  if (texts.length === 0) {
    const upstream = path.join(vendored, `${id}.txt`);
    if (!fs.existsSync(upstream))
      throw new Error(`${id} ships no license file; vendor its upstream text at ${upstream}`);
    texts.push(fs.readFileSync(upstream, "utf8").trim());
  }
  const manifest = JSON.parse(fs.readFileSync(path.join(pkg.path, "package.json"), "utf8"));
  const own = new Set([...texts, `Copyright ${authorName(manifest.author)}`].flatMap((text) => [...holders(text)]));
  // Only the runtime code of build tools reaches the output, so their own files are not scanned.
  const notices = pkg.injected ? [] : fileNotices(pkg, own);
  const title = pkg.injected ? `${id}: ${pkg.license}, for the runtime code the build adds` : `${id}: ${pkg.license}`;
  let section = `${rule}\n${title}\n${rule}\n\n${texts.join("\n\n")}`;
  if (notices.length > 0) {
    const parts = notices.map(
      ({ text, labels }) => `From:\n${labels.map((label) => `  ${label}\n`).join("")}\n${text}`,
    );
    for (const credit of creditedNames(id, notices))
      parts.push(subsection("The license credited above", [creditedText(credit)]));
    section += `\n\n${subsection(`Notices and credits in ${id}'s files`, parts)}`;
  }
  sections.push(section);
}
if (uncredited.length > 0)
  throw new Error(`Add these credited projects' license texts to \`credited\`:\n\n${uncredited.join("\n\n")}`);
process.stdout.write(`${sections.join("\n\n")}\n`);
