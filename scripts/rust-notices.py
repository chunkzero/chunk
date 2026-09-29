#!/usr/bin/env python3
"""Print the third-party notices for the shipped `chunk` binary, or refresh the upstream texts they vendor.

The notices carry every license, copyright and NOTICE file in the source of each crate `chunk` links, plus the
license notices in its source file comments, followed by licenses/v8.txt for the V8 build the `v8` crate links. Crates
published without any license file use the upstream text vendored at licenses/crates/<name>-<version>.txt, and
license texts that source notices refer to are vendored under licenses/referenced. `--vendor` refreshes all vendored
texts and licenses/v8.txt from git; printing works offline from the local Cargo registry.
"""

import argparse
import configparser
import fnmatch
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import textwrap
import tomllib

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
COPYRIGHT = re.compile(r"\b(Copyright|COPYRIGHT)(\s*(\(c\)|\(C\)|©|ⓒ))*\s+(?!(Notice|NOTICE|HOLDER|OWNER))[\dA-Z]|©"
                       r"|SPDX-FileCopyrightText:")
GRANT = re.compile(r"permission is hereby granted|redistribution and use|permission to use, copy, modify"
                   r"|licensed under|under the terms of|governed by|SPDX-License-Identifier:\s*\w"
                   r"|\b(MIT|BSD|ISC|Apache|zlib|Boost|MPL)\b[\w .-]{0,20}licen[cs]e", re.IGNORECASE)
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
# License texts that source notices point to without their source shipping them: (holder, name, repository, ref, path).
# They are vendored at licenses/referenced/<name>.txt and printed after the notices that name the holder.
REFERENCED_LICENSES = [
    ("The Chromium Authors", "chromium", "https://chromium.googlesource.com/chromium/src", "refs/tags/150.0.7871.0",
     "LICENSE"),
    ("The Go Authors", "go", "https://github.com/golang/go", "refs/tags/go1.27.1", "LICENSE"),
    ("the Dart project authors", "dart", "https://github.com/dart-lang/sdk", "refs/tags/3.13.5", "LICENSE"),
    ("Domenic Denicola", "webidl-conversions", "https://github.com/jsdom/webidl-conversions", "refs/tags/v8.0.1",
     "LICENSE.md"),
]


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
    """The text of each `/* */` comment and each run of `//`, `///` or `//!` lines that starts a line."""
    blocks, block, kind = [], [], None
    for line in source.splitlines() + [""]:
        stripped = line.strip()
        if kind == "/*":
            content, closed, _ = line.partition("*/")
            block.append(re.sub(r"^\s*\*(?!/) ?", "", content))
            if not closed:
                continue
            kind = None
        else:
            marker = re.match(r"//(/(?!/)|!)?", stripped)
            if block and (not marker or marker[0] != kind):
                blocks.append(block)
                block = []
            if marker:
                block.append(stripped.removeprefix(marker[0]).removeprefix(" "))
                kind = marker[0]
                continue
            kind = None
            if not stripped.startswith("/*"):
                continue
            content, closed, _ = stripped[2:].partition("*/")
            block.append(content.lstrip("*!").strip())
            if not closed:
                kind = "/*"
                continue
        blocks.append(block)
        block = []
    return [textwrap.dedent("\n".join(block)).strip() for block in blocks]


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


def source_notices(sources, own):
    """Distinct license notices in the comments of `sources`, as (text, paths) pairs.

    A notice is a comment with both a copyright line and license terms. Notices whose copyright holders are all in
    `own`, the holders the included license files and package authors name, restate those files and are left out.
    """
    notices = {}
    for path, source in sources:
        for block in comment_blocks(source):
            if not (COPYRIGHT.search(block) and GRANT.search(block)):
                continue
            named = holders(block)
            if named and named <= own:
                continue
            _, paths = notices.setdefault(" ".join(block.lower().split()), (block, []))
            if path not in paths:
                paths.append(path)
    return sorted(notices.values(), key=lambda notice: notice[1][0])


def notice_section(title, notices):
    """The notices, followed by the vendored license texts they point to."""
    parts = ["From:\n" + "".join(f"  {path}\n" for path in paths) + f"\n{text}" for text, paths in notices]
    for holder, name, *_ in REFERENCED_LICENSES:
        if any(holder.lower() in text.lower() for text, _ in notices):
            referenced = REPOSITORY / "licenses/referenced" / f"{name}.txt"
            if not referenced.exists():
                raise ValueError(f"licenses/referenced/{name}.txt is missing; run scripts/rust-notices.py --vendor")
            parts.append(f"{'-' * 80}\nThe license the notices above refer to for {holder}\n{'-' * 80}\n\n"
                         f"{referenced.read_text().strip()}")
    return f"{RULE}\nNotices from source files in {title}\n{RULE}\n\n" + "\n\n".join(parts)


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
    for package in shipped_crates():
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
        if found:
            headers.append(notice_section(name, found))
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
        git(repository, "init", "--quiet", "--bare")

    def commit(self, url, ref):
        if (url, ref) not in self.commits:
            if url not in self.remotes:
                self.remotes[url] = f"remote{len(self.remotes)}"
                git(self.repository, "remote", "add", self.remotes[url], url)
            git(self.repository, "fetch", "--quiet", "--depth", "1", "--filter=blob:none", self.remotes[url], ref)
            self.commits[url, ref] = git(self.repository, "rev-parse", "FETCH_HEAD^{commit}").strip()
        return self.commits[url, ref]

    def license_files(self, commit, directory):
        listed = git(self.repository, "ls-tree", "--name-only", commit, *([f"{directory}/"] if directory else []))
        names = [Path(path).name for path in listed.splitlines()]
        return sorted(name for name in names
                      if LICENSE_FILE.fullmatch(name) and not SOURCE_FILE.fullmatch(name)
                      and not name.lower().endswith(".html"))

    def show(self, commit, path):
        return git(self.repository, "show", f"{commit}:{path}").strip()

    def sources(self, url, commit, directory, skipped):
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
        oids = "".join(f"{oid}\n" for oid in blobs.values()).encode()
        subprocess.run(["git", "-C", self.repository, "-c", "fetch.negotiationAlgorithm=noop", "fetch", "--quiet",
                        "--no-tags", "--no-write-fetch-head", "--filter=blob:none", self.remotes[url], "--stdin"],
                       input=oids, check=True, capture_output=True)
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
        commit = upstream.commit(url, ref)
        (directory / f"{name}.txt").write_text(section(url, commit, path, upstream.show(commit, path)) + "\n")


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
        found = source_notices(upstream.sources(url, commit, directory, set() if directory else {"third_party"}), own)
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
