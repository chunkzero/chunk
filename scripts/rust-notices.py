#!/usr/bin/env python3
"""Print the third-party notices for the shipped `chunk` binary, or refresh the upstream texts they vendor.

The notices carry every license, copyright and NOTICE file in the source of each crate `chunk` links, plus the
license notices in its source file comments and the licenses of third-party code it embeds, followed by
licenses/v8.txt for the V8 build the `v8` crate links. Crates published without any license file use the upstream text
vendored at licenses/crates/<name>-<version>.txt, and license texts that source notices or embedded code refer to are
vendored under licenses/referenced. `--vendor` refreshes all vendored
texts and licenses/v8.txt from upstream; printing works offline from the local Cargo registry.
"""

import argparse
import configparser
import fnmatch
import io
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile
import textwrap
import tomllib
import urllib.request

REPOSITORY = Path(__file__).resolve().parent.parent
TARGET = "x86_64-unknown-linux-gnu"
LICENSE_FILE = re.compile(r"((third[-_]party[-_])?(licen[cs]es?|notices?)|copying|copyright|unlicense)([-_.][\w.-]*)?",
                          re.IGNORECASE)
SOURCE_FILE = re.compile(r".*\.(rs|c|cc|h|hh|cpp|py|js|ts|json|toml)$", re.IGNORECASE)
SKIPPED_DIRECTORIES = {"tests", "benches", "examples", "assets", "docs", "fuzz", ".github"}
# Source files whose comments are scanned for license notices, except tests and uncompiled directories.
SCANNED_FILE = re.compile(r".*\.(rs|js|mjs|ts|c|cc|cpp|h|hh|hpp|inc|s|asm|tq)", re.IGNORECASE)
TEST_FILE = re.compile(r"((.*[_-])?(unit)?tests?|test_.*|.*_fuzzer|.*_benchmark)\.\w+")
UNCOMPILED_DIRECTORIES = SKIPPED_DIRECTORIES | {"test", "testing", "testdata", "fixtures", "bench", "benchmark",
                                                "benchmarks", "fuzzers", "fuzzing", "doc", "samples", "tools"}
COMMENT_START = re.compile(r"/\*|(?<![:/\\])//(/(?!/)|!)?")
COPYRIGHT = re.compile(r"\b(Copyright|COPYRIGHT)(\s*(\(c\)|\(C\)|©|ⓒ))*\s+(?!(Notice|NOTICE|HOLDER|OWNER))[\dA-Z]|©"
                       r"|SPDX-FileCopyrightText:")
GRANT = re.compile(r"permission is hereby granted|redistribution and use|permission to use, copy, modify"
                   r"|licensed under|under the terms of|governed by|SPDX-License-Identifier:\s*\w"
                   r"|\b(MIT|BSD|ISC|Apache|zlib|Boost|MPL)\b[\w\s.-]{0,20}licen[cs]e"
                   r"|licen[cs]e\W{1,3}(MIT|BSD|ISC|Apache|zlib|Boost|MPL)\b", re.IGNORECASE)
# Files that do not apply to the build: a license alternative we do not elect, or code the build leaves out.
EXCLUDED = {
    "self_cell": ["LICENSE-GPLv2"],
    "libsqlite3-sys": ["sqlcipher/*"],
    "v8": ["*/*"],  # licenses/v8.txt covers the bundled V8 sources.
}
RULE = "=" * 80

RUSTY_V8 = "https://github.com/denoland/rusty_v8"
# Components compiled into the prebuilt V8 static library: (name, rusty_v8 submodule, directory in it).
V8_COMPONENTS = [
    ("rusty_v8", "", ""),
    ("V8", "v8", ""),
    ("V8: glibc trigonometric functions", "v8", "third_party/glibc"),
    ("V8: inspector protocol", "v8", "third_party/inspector_protocol"),
    ("V8: rapidhash", "v8", "third_party/rapidhash-v8"),
    ("V8: SipHash", "v8", "third_party/siphash"),
    ("V8: UTF-8 decoder", "v8", "third_party/utf8-decoder"),
    ("V8: array sort builtins", "v8", "third_party/v8/builtins"),
    ("V8: fp16 code generation", "v8", "third_party/v8/codegen"),
    ("V8: Valgrind client API", "v8", "third_party/valgrind"),
    ("ICU", "third_party/icu", ""),
    ("Abseil", "third_party/abseil-cpp", ""),
    ("libc++", "third_party/libc++/src", ""),
    ("libc++abi", "third_party/libc++abi/src", ""),
    ("LLVM libc", "third_party/llvm-libc/src", ""),
    ("simdutf", "third_party/simdutf", ""),
    ("Highway", "third_party/highway/src", ""),
    ("fast_float", "third_party/fast_float/src", ""),
    ("Dragonbox", "third_party/dragonbox/src", ""),
    ("FP16", "third_party/fp16/src", ""),
]
# The copyright holder of components whose license files name none, for telling their own source notices apart.
V8_AUTHORS = {"Abseil": "The Abseil Authors"}
# License texts that source notices or embedded code refer to without their source shipping them: (marker, name,
# repository, ref, path), or (marker, name, archive URL, None, path in it) for upstreams no longer in a public
# repository. They are vendored at licenses/referenced/<name>.txt and printed after the notices that contain their
# marker, or after the EMBEDDED statements that name them.
REFERENCED_LICENSES = [
    ("The Chromium Authors", "chromium", "https://chromium.googlesource.com/chromium/src", "refs/tags/150.0.7871.0",
     "LICENSE"),
    ("The Go Authors", "go", "https://github.com/golang/go", "refs/tags/go1.27.1", "LICENSE"),
    ("the Dart project authors", "dart", "https://github.com/dart-lang/sdk", "refs/tags/3.13.5", "LICENSE"),
    ("Domenic Denicola", "webidl-conversions", "https://github.com/jsdom/webidl-conversions", "refs/tags/v8.0.1",
     "LICENSE.md"),
    ("facebook/regenerator", "regenerator", "https://github.com/facebook/regenerator", "refs/tags/v0.14.1", "LICENSE"),
    ("Finagle", "finagle", "https://github.com/twitter/finagle", "9cc08d15216497bb03a1cafda96b7266cfbbcff1", "NOTICE"),
    # The Babel release oxc's sources cite.
    (None, "babel", "https://github.com/babel/babel", "refs/tags/v7.26.2", "LICENSE"),
    (None, "babel-plugin-styled-components", "https://github.com/styled-components/babel-plugin-styled-components",
     "refs/tags/v2.3.0", "LICENSE.md"),
    (None, "brotli", "https://github.com/google/brotli", "refs/tags/v1.1.0", "LICENSE"),
    (None, "browserslist", "https://github.com/browserslist/browserslist", "refs/tags/4.28.8", "LICENSE"),
    (None, "bumpalo", "https://github.com/fitzgen/bumpalo", "a47f6d6b7b5fee9c99a285f0de80257a0a982ef3", "LICENSE-MIT"),
    (None, "caniuse-lite", "https://github.com/browserslist/caniuse-lite", "refs/tags/1.0.30001809", "LICENSE"),
    (None, "coloriz", "https://crates.io/api/v1/crates/coloriz/0.2.0/download", None, "coloriz-0.2.0/LICENSE"),
    (None, "compat-table", "https://github.com/compat-table/compat-table", "a970fc00cc33b58d0b84d4b290ea46a185c8fcf1",
     "LICENSE"),
    (None, "electron-to-chromium", "https://github.com/Kilian/electron-to-chromium", "refs/tags/v1.5.403", "LICENSE"),
    (None, "enhanced-resolve", "https://github.com/webpack/enhanced-resolve", "refs/tags/v5.26.0", "LICENSE"),
    (None, "mime_more", "https://github.com/7086cmd/mime_more", "f9aed559f695331db7a0bbd200501424c804c1a7", "LICENSE"),
    (None, "node", "https://github.com/nodejs/node", "refs/tags/v24.9.0", "LICENSE"),
    (None, "node-releases", "https://github.com/chicoxyzzy/node-releases", "refs/tags/v2.0.53", "LICENSE"),
    (None, "parcel", "https://github.com/parcel-bundler/parcel", "refs/tags/v2.16.4", "LICENSE"),
    (None, "protobuf", "https://github.com/protocolbuffers/protobuf", "refs/tags/v25.8", "LICENSE"),
    (None, "rust_urlencoding", "https://github.com/kornelski/rust_urlencoding",
     "a617c89d16f390e3ab4281ea68c514660b111301", "LICENSE"),
    (None, "tsconfck", "https://registry.npmjs.org/tsconfck/-/tsconfck-3.1.6.tgz", None, "package/LICENSE"),
    (None, "tsconfig-paths", "https://github.com/dividab/tsconfig-paths", "refs/tags/v4.2.0", "LICENSE"),
    (None, "tz-rs", "https://github.com/x-hgg-x/tz-rs", "refs/tags/v0.6.14", "LICENSE-MIT"),
    (None, "zmij", "https://github.com/vitaut/zmij", "refs/tags/v1.2", "LICENSE"),
]
# Where a vendored text is cut, before the parts that do not apply, such as Node.js's list of its bundled libraries.
EXCERPTS = {"node": "The externally maintained libraries used by Node.js are:"}
# Third-party code that crates embed with the attribution only in documentation: "<name> <version>" -> statements of
# what it embeds, each with the REFERENCED_LICENSES text that covers it.
EMBEDDED = {
    "brotli 6.0.0": [("Its README calls it a direct port of Google's C brotli compressor.", "brotli")],
    "brotli-decompressor 4.0.3": [("Its README calls it a direct port of Google's C brotli decompressor.", "brotli")],
    "chrono 0.4.45": [("src/offset/local/tz_info is forked from the tz-rs crate.", "tz-rs")],
    "deno_core 0.411.0": [
        ("02_timers.js copies Node.js's internal linked list and priority queue, and 01_core.js mirrors its task "
         "queues.", "node"),
    ],
    "nu-ansi-term 0.50.3": [("src/rgb.rs is borrowed from the coloriz crate.", "coloriz")],
    "oxc-browserslist 5.0.1": [
        ("src/generated embeds browser usage and support data from caniuse-lite 1.0.30001809, the caniuse.com data by "
         "Alexis Deveria, licensed under CC BY 4.0.", "caniuse-lite"),
        ("src/generated embeds Electron release data from electron-to-chromium 1.5.403.", "electron-to-chromium"),
        ("src/generated embeds Node.js release data from node-releases 2.0.53.", "node-releases"),
        ("Its README calls it a Rust port of Browserslist, at 4.28.8 for this release.", "browserslist"),
    ],
    "oxc_allocator 0.149.0": [("src/arena and src/vec2 are derived from bumpalo.", "bumpalo")],
    "oxc_compat 0.149.0": [
        ("src/es_features.rs is generated from compat-table data by scripts adapted from Babel's babel-compat-data.",
         "compat-table"),
        ("src/es_features.rs is generated by scripts adapted from Babel's babel-compat-data.", "babel"),
    ],
    "oxc_ecmascript 0.149.0": [
        ("src/constant_evaluation/url_encoding is based on the rust_urlencoding crate.", "rust_urlencoding"),
    ],
    "oxc_minifier 0.149.0": [("src/traverse_context/uid.rs is based on Babel's scope.generateUid.", "babel")],
    "oxc_resolver 11.24.3": [
        ("Its README says it partially copies code from webpack/enhanced-resolve.", "enhanced-resolve"),
        ("Its README says it partially copies code from dividab/tsconfig-paths.", "tsconfig-paths"),
        ("Its README says it partially copies code from parcel-bundler/parcel.", "parcel"),
        ("Its README says it partially copies code from dominikg/tsconfck.", "tsconfck"),
    ],
    "oxc_transformer 0.149.0": [
        ("Its transforms are based on Babel's plugins, as their module documentation says.", "babel"),
        ("src/plugins/styled_components.rs is a port of the styled-components Babel plugin.",
         "babel-plugin-styled-components"),
    ],
    "oxc_traverse 0.149.0": [("src/ast_operations/gather_node_parts.rs is ported from @babel/traverse.", "babel")],
    "prost-types 0.14.4": [
        ("src/protobuf.rs is generated from the Protocol Buffers well-known types, which its README says are included "
         "under their original BSD license.", "protobuf"),
    ],
    "rolldown_plugin_oxc_runtime 1.2.8": [
        ("src/generated/embedded_helpers.rs embeds the helpers of @oxc-project/runtime 0.149.0, whose README says they "
         "are copied from @babel/runtime.", "babel"),
    ],
    "rolldown_utils 1.2.8": [("src/light_guess.rs is ported from the mime_more crate.", "mime_more")],
    "zmij 1.0.23": [("It is a line-by-line port of Victor Zverovich's C++ zmij.", "zmij")],
}

def cargo(*args):
    return subprocess.run(["cargo", *args, "--locked"], cwd=REPOSITORY, check=True, capture_output=True,
                          text=True).stdout


def shipped_crates():
    """Registry packages compiled into `chunk` for the release target, excluding build and dev dependencies."""
    tree = cargo("tree", "-p", "chunk-cli", "-e", "normal", "--target", TARGET, "--prefix", "none", "-f", "{p}")
    linked = {match.groups() for match in map(re.compile(r"(\S+) v(\S+)").match, tree.splitlines()) if match}
    packages = json.loads(cargo("metadata", "--format-version", "1"))["packages"]
    return sorted((package for package in packages
                   if package["source"] and (package["name"], package["version"]) in linked),
                  key=lambda package: (package["name"], package["version"]))


def crate_files(package):
    """Paths of the files in a crate's source outside SKIPPED_DIRECTORIES and its EXCLUDED patterns."""
    root = Path(package["manifest_path"]).parent
    excluded = EXCLUDED.get(package["name"], [])
    for directory, subdirectories, files in os.walk(root):
        subdirectories[:] = [name for name in subdirectories if name not in SKIPPED_DIRECTORIES]
        for name in files:
            path = Path(directory, name).relative_to(root).as_posix()
            if not any(fnmatch.fnmatch(path, pattern) for pattern in excluded):
                yield path


def license_files(package):
    found = [path for path in crate_files(package)
             if LICENSE_FILE.fullmatch(Path(path).name) and not SOURCE_FILE.fullmatch(Path(path).name)]
    return sorted(found, key=lambda path: (path.count("/"), path))


def scanned(path):
    *directories, name = path.split("/")
    return SCANNED_FILE.fullmatch(name) and not TEST_FILE.fullmatch(name) \
        and not UNCOMPILED_DIRECTORIES.intersection(directories)


def comment_blocks(source):
    """The text of each `/* */` comment and each run of `//`, `///` or `//!` comments, wherever they start in a line.

    String literals are not skipped, so comments in code embedded as text are found too. A line comment that starts a
    line continues a run of the same kind from the line above. Each block comes with whether only whitespace separates
    it from the one before.
    """
    blocks, block, kind, adjacent, code = [], [], None, False, True

    def flush():
        nonlocal block, kind
        if block:
            blocks.append((block, adjacent))
        block, kind = [], None

    def start():
        nonlocal adjacent, code
        if not block:
            adjacent, code = not code, False

    for line in source.splitlines():
        rest, leading = line, True
        if kind == "/*":
            content, closed, rest = line.partition("*/")
            block.append(re.sub(r"^\s*\*(?!/) ?", "", content))
            if not closed:
                continue
            flush()
            leading = False
        while match := COMMENT_START.search(rest):
            content = rest[match.end():]
            if rest[:match.start()].strip():
                code = True
            if match[0] != "/*":
                if not (leading and kind == match[0] and not rest[:match.start()].strip()):
                    flush()
                start()
                block.append(content.removeprefix(" "))
                kind = match[0]
                break
            flush()
            start()
            content, closed, rest = content.partition("*/")
            block.append(content.lstrip("*!").strip())
            if not closed:
                kind = "/*"
                break
            flush()
            leading = False
        else:
            if rest.strip():
                code = True
            flush()
    if kind != "/*":
        flush()
    return [(textwrap.dedent("\n".join(line.rstrip() for line in block)).strip(), adjacent)
            for block, adjacent in blocks]


def holders(text):
    """Normalized copyright holders named in a text, such as "brian smith" for "Copyright 2015-2016 Brian Smith."."""
    found = set()
    for line in text.splitlines():
        match = COPYRIGHT.search(line)
        if match:
            statement = re.split(r"\.\s+(?=[A-Z])", line[match.start():])[0]
            statement = re.sub(r"(?i)(spdx-file)?copyright(text)?|\(c\)|all rights reserved|\d{4}|[^\w\s]", " ",
                               statement)
            words = statement.lower().split()
            found.add(" ".join(words[1:] if words[:1] == ["the"] else words))
    return found - {""}


def notice_blocks(blocks):
    """The comment blocks with both a copyright line and license terms, pairing adjacent blocks that each have one."""
    def complete(text):
        return bool(COPYRIGHT.search(text) and GRANT.search(text))

    found = []
    for index, (text, adjacent) in enumerate(blocks):
        if complete(text):
            found.append(text)
        elif adjacent and not complete(blocks[index - 1][0]) and complete(f"{blocks[index - 1][0]}\n\n{text}"):
            found.append(f"{blocks[index - 1][0]}\n\n{text}")
    return found


def source_notices(sources, own):
    """Distinct license notices in the comments of `sources`, as (text, paths) pairs.

    A notice is a comment with both a copyright line and license terms. Notices whose copyright holders are all in
    `own`, the holders the included license files and package authors name, restate those files and are left out.
    """
    notices = {}
    for path, source in sources:
        for block in notice_blocks(comment_blocks(source)):
            named = holders(block)
            if named and named <= own:
                continue
            _, paths = notices.setdefault(" ".join(block.lower().split()), (block, []))
            if path not in paths:
                paths.append(path)
    # A notice that another one quotes in full is left to that one.
    for key in [key for key in notices if any(key != other and key in other for other in notices)]:
        _, paths = notices.pop(key)
        container = next(other for other in notices if key in other)
        notices[container][1].extend(path for path in paths if path not in notices[container][1])
    return sorted(notices.values(), key=lambda notice: notice[1][0])


def notice_section(title, notices, embedded=()):
    """The notices and EMBEDDED statements, followed by the vendored license texts they refer to."""
    parts = ["From:\n" + "".join(f"  {path}\n" for path in paths) + f"\n{text}" for text, paths in notices]
    parts += [statement for statement, _ in embedded]
    named = {name for _, name in embedded}
    for marker, name, url, *_ in REFERENCED_LICENSES:
        if name in named or marker and any(marker.lower() in text.lower() for text, _ in notices):
            referenced = REPOSITORY / "licenses/referenced" / f"{name}.txt"
            if not referenced.exists():
                raise ValueError(f"licenses/referenced/{name}.txt is missing; run scripts/rust-notices.py --vendor")
            parts.append(f"{'-' * 80}\nThe license referred to above, from {url}\n{'-' * 80}\n\n"
                         f"{referenced.read_text().strip()}")
    return f"{RULE}\nNotices from the source of {title}\n{RULE}\n\n" + "\n\n".join(parts)


def vendored(package):
    return REPOSITORY / "licenses/crates" / f'{package["name"]}-{package["version"]}.txt'


def notices():
    lock = tomllib.loads((REPOSITORY / "Cargo.lock").read_text())
    [v8] = [package["version"] for package in lock["package"] if package["name"] == "v8"]
    v8_notices = (REPOSITORY / "licenses/v8.txt").read_text()
    if f"`v8` crate {v8} (" not in v8_notices.partition("\n")[0]:
        raise ValueError(f"licenses/v8.txt does not match v8 {v8}; run scripts/rust-notices.py --vendor")
    crates = []
    texts = {}
    headers = []
    packages = shipped_crates()
    for package in packages:
        name = f'{package["name"]} {package["version"]}'
        source = f'https://crates.io/crates/{package["name"]}/{package["version"]}'
        if package["repository"]:
            source += f', {package["repository"]}'
        crates.append(f'  {name} ({package["license"]}): {source}')
        root = Path(package["manifest_path"]).parent
        files = [(path, (root / path).read_text(errors="replace")) for path in license_files(package)]
        if not files:
            if not vendored(package).exists():
                raise ValueError(f"{name} ships no license file; run scripts/rust-notices.py --vendor")
            files = [(f"none published; {vendored(package).relative_to(REPOSITORY)}", vendored(package).read_text())]
        for path, text in files:
            texts.setdefault(text.replace("\r\n", "\n").strip(), []).append(f"{name}: {path}")
        own = set().union(*(holders(text) for _, text in files),
                          *(holders("Copyright " + re.sub(r"<.*?>", "", author)) for author in package["authors"]))
        sources = ((path, (root / path).read_text(errors="replace"))
                   for path in sorted(crate_files(package)) if scanned(path))
        found = source_notices(sources, own)
        embedded = EMBEDDED.get(name, [])
        if found or embedded:
            headers.append(notice_section(name, found, embedded))
    stale = set(EMBEDDED) - {f'{package["name"]} {package["version"]}' for package in packages}
    if stale:
        raise ValueError(f"EMBEDDED names crates chunk no longer links; recheck what they embed: {sorted(stale)}")
    sections = [
        "Third-party notices for the chunk CLI.",
        "The chunk binary statically links the Rust crates below. The source of each, including the Source Code "
        "Form of the MPL-2.0 crates, is available from its crates.io page and upstream repository.",
        "Crates:\n" + "\n".join(crates),
    ]
    for text, users in sorted(texts.items(), key=lambda item: item[1][0]):
        sections.append(f"{RULE}\nShipped with:\n" + "".join(f"  {user}\n" for user in users) + f"{RULE}\n\n{text}")
    sections += headers
    return "\n\n".join(sections) + "\n\n\n" + v8_notices


def git(repository, *args):
    return subprocess.run(["git", "-C", repository, *args], check=True, capture_output=True, text=True).stdout


class Upstream:
    """Shallow, blob-less fetches of upstream commits into one scratch repository."""

    def __init__(self, repository):
        self.repository = repository
        self.commits = {}
        self.remotes = {}
        self.origins = {}
        git(repository, "init", "--quiet", "--bare")

    def commit(self, url, ref):
        if (url, ref) not in self.commits:
            if url not in self.remotes:
                self.remotes[url] = f"remote{len(self.remotes)}"
                git(self.repository, "remote", "add", self.remotes[url], url)
            git(self.repository, "fetch", "--quiet", "--depth", "1", "--filter=blob:none", self.remotes[url], ref)
            self.commits[url, ref] = git(self.repository, "rev-parse", "FETCH_HEAD^{commit}").strip()
            self.origins[self.commits[url, ref]] = self.remotes[url]
        return self.commits[url, ref]

    def fetch_blobs(self, commit, oids):
        """Fetch blobs from the remote `commit` came from; git's own lazy fetches may ask another remote and hang."""
        subprocess.run(["git", "-C", self.repository, "-c", "fetch.negotiationAlgorithm=noop", "fetch", "--quiet",
                        "--no-tags", "--no-write-fetch-head", "--filter=blob:none", self.origins[commit], "--stdin"],
                       input="".join(f"{oid}\n" for oid in oids).encode(), check=True, capture_output=True)

    def license_files(self, commit, directory):
        listed = git(self.repository, "ls-tree", "--name-only", commit, *([f"{directory}/"] if directory else []))
        names = [Path(path).name for path in listed.splitlines()]
        return sorted(name for name in names
                      if LICENSE_FILE.fullmatch(name) and not SOURCE_FILE.fullmatch(name)
                      and not name.lower().endswith(".html"))

    def show(self, commit, path):
        oid = git(self.repository, "rev-parse", f"{commit}:{path}").strip()
        self.fetch_blobs(commit, [oid])
        return git(self.repository, "cat-file", "blob", oid).strip()

    def sources(self, commit, directory, skipped):
        """(path, text) of each scanned source file under `directory` outside uncompiled and `skipped` directories."""
        listed = git(self.repository, "ls-tree", "-r", "-z", commit, *([f"{directory}/"] if directory else []))
        blobs = {}
        for entry in filter(None, listed.split("\0")):
            info, path = entry.split("\t", 1)
            _, kind, oid = info.split()
            if kind == "blob" and scanned(path) and not skipped.intersection(path.split("/")[:-1]):
                blobs[path] = oid
        if not blobs:
            return []
        self.fetch_blobs(commit, blobs.values())
        oids = "".join(f"{oid}\n" for oid in blobs.values()).encode()
        batch = subprocess.run(["git", "-C", self.repository, "cat-file", "--batch"], input=oids, check=True,
                               capture_output=True).stdout
        sources, position = [], 0
        for path in blobs:
            header = batch.index(b"\n", position)
            size = int(batch[position:header].split()[2])
            sources.append((path, batch[header + 1:header + 1 + size].decode(errors="replace")))
            position = header + 2 + size
        return sources


def section(url, commit, path, text):
    return f"Source: {url} at {commit}, {path}\n\n{text}"


def vendor_crates(upstream):
    """Vendor the nearest upstream license files of each shipped crate that publishes none.

    A crate whose upstream has no license file either keeps a text assembled by hand from its upstream's own license
    statement, starting with "Assembled by hand", or is reported.
    """
    packages = shipped_crates()
    commits = {}
    for package in packages:
        info = Path(package["manifest_path"]).with_name(".cargo_vcs_info.json")
        if info.exists():
            vcs = json.loads(info.read_text())
            package["vcs"] = (vcs["git"]["sha1"], vcs.get("path_in_vcs", ""))
            if package["repository"]:
                commits[vcs["git"]["sha1"]] = package["repository"]
    directory = REPOSITORY / "licenses/crates"
    directory.mkdir(exist_ok=True)
    wanted = set()
    unresolved = []
    for package in packages:
        if license_files(package):
            continue
        name = f'{package["name"]} {package["version"]}'
        target = vendored(package)
        wanted.add(target.name)
        if target.exists() and target.read_text().startswith("Assembled by hand"):
            continue
        if "vcs" not in package:
            unresolved.append(f"{name}: no license file and no VCS revision")
            continue
        sha, path = package["vcs"]
        url = commits.get(sha) or package["repository"]
        github = re.match(r"https://github\.com/([^/]+)/([^/#?]+?)(?:\.git)?(?:[/#?]|$)", url or "")
        url = f"https://github.com/{github[1]}/{github[2]}" if github else url
        commit = upstream.commit(url, sha)
        parts = Path(path).parts
        for depth in range(len(parts), -1, -1):
            prefix = "/".join(parts[:depth])
            files = upstream.license_files(commit, prefix)
            if files:
                break
        else:
            unresolved.append(f"{name}: no license file in {url} at {commit}")
            continue
        paths = [f"{prefix}/{file}" if prefix else file for file in files]
        target.write_text("\n\n".join(
            section(url, commit, file, upstream.show(commit, file)) for file in paths) + "\n")
    for stale in directory.iterdir():
        if stale.name not in wanted:
            stale.unlink()
    if unresolved:
        raise ValueError("Assemble these crates' license texts by hand in licenses/crates:\n" + "\n".join(unresolved))


def vendor_referenced(upstream):
    directory = REPOSITORY / "licenses/referenced"
    directory.mkdir(exist_ok=True)
    for stale in directory.iterdir():
        stale.unlink()
    for _, name, url, ref, path in REFERENCED_LICENSES:
        if ref:
            commit = upstream.commit(url, ref)
            text = section(url, commit, path, upstream.show(commit, path))
        else:
            with urllib.request.urlopen(url) as response, \
                    tarfile.open(fileobj=io.BytesIO(response.read())) as archive:
                text = f"Source: {url}, {path}\n\n{archive.extractfile(path).read().decode().strip()}"
        if name in EXCERPTS:
            text = text.partition(EXCERPTS[name])[0].strip()
        (directory / f"{name}.txt").write_text("".join(f"{line.rstrip()}\n" for line in text.splitlines()))


def vendor_v8(upstream):
    lock = tomllib.loads((REPOSITORY / "Cargo.lock").read_text())
    [version] = [package["version"] for package in lock["package"] if package["name"] == "v8"]
    tag = f"v{version}"
    tree = upstream.commit(RUSTY_V8, f"refs/tags/{tag}")
    modules = configparser.ConfigParser()
    modules.read_string(git(upstream.repository, "show", f"{tree}:.gitmodules"))
    sections = [
        f"Third-party notices for V8 as linked by the `v8` crate {version} ({RUSTY_V8}/tree/{tag}).",
        "Generated by scripts/rust-notices.py from the license files and the license notices in the source files "
        "of each component.",
    ]
    for name, submodule, directory in V8_COMPONENTS:
        url, commit = RUSTY_V8, tree
        if submodule:
            url = modules[f'submodule "{submodule}"']["url"]
            commit = upstream.commit(url, git(upstream.repository, "ls-tree", tree, submodule).split()[2])
        files = upstream.license_files(commit, directory)
        if not files:
            raise ValueError(f"No license file for {name} in {url} {directory}")
        own = holders(f"Copyright {V8_AUTHORS[name]}") if name in V8_AUTHORS else set()
        for file in files:
            path = f"{directory}/{file}" if directory else file
            text = upstream.show(commit, path)
            own |= holders(text)
            sections.append(f"{RULE}\n{name}\nSource: {url} at {commit}, {path}\n{RULE}\n\n{text}")
        # A repository's own third_party code is linked only where V8_COMPONENTS lists its directory.
        found = source_notices(upstream.sources(commit, directory, set() if directory else {"third_party"}), own)
        if found:
            sections.append(notice_section(f"{name} ({url} at {commit})", found))
    (REPOSITORY / "licenses/v8.txt").write_text("\n\n".join(sections) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--vendor", action="store_true", help="refresh the vendored upstream texts from git")
    if parser.parse_args().vendor:
        with tempfile.TemporaryDirectory(prefix="rust-notices-") as repository:
            upstream = Upstream(repository)
            vendor_crates(upstream)
            vendor_referenced(upstream)
            vendor_v8(upstream)
    else:
        print(notices(), end="")


if __name__ == "__main__":
    main()
