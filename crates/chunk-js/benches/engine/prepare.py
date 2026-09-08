#!/usr/bin/env python3
"""Extract an unchanged engine revision for this temporary comparison."""
import os
import pathlib
import shutil
import subprocess

HERE = pathlib.Path(__file__).resolve().parent
ROOT = subprocess.check_output(["git", "rev-parse", "--show-toplevel"], cwd=HERE, text=True).strip()
REVISION = os.environ.get("BENCH_REVISION", "97f0ee1")
PREFIX = "crates/chunk-js/"
destination = HERE / "generated-baseline"
paths = subprocess.check_output(
    ["git", "ls-tree", "-r", "--name-only", REVISION, PREFIX], cwd=ROOT, text=True
).splitlines()
# This ignored extraction is disposable; remove stale modules from newer APIs.
if destination.exists():
    shutil.rmtree(destination)
for path in paths:
    relative = path.removeprefix(PREFIX)
    content = subprocess.check_output(["git", "show", f"{REVISION}:{path}"], cwd=ROOT)
    if relative == "Cargo.toml":
        content = content.decode().replace('name = "chunk-js"', 'name = "chunk-js-baseline"')
        for key, value in {
            "version": '"0.0.0"', "edition": '"2024"',
            "rust-version": '"1.98"', "license": '"FSL-1.1-MIT"',
        }.items():
            content = content.replace(f"\n{key}.workspace = true", f"\n{key} = {value}")
        content = content.replace("[lints]\nworkspace = true", '[lints.rust]\nunsafe_code = "forbid"')
        content = content.encode()
    target = destination / relative
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_bytes(content)
(destination / "REVISION").write_text(subprocess.check_output(
    ["git", "rev-parse", REVISION], cwd=ROOT, text=True
))
print(f"Prepared unchanged baseline sources from {REVISION} in {destination}")
