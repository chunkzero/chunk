#!/usr/bin/env python3
"""Plan a nightly CLI version, publish its verified archives as a GitHub prerelease, and keep the latest 30."""

import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import tomllib

NIGHTLY = re.compile(r"v\d+\.\d+\.\d+-nightly\.\d{8}\.g[0-9a-f]{12}")
PLATFORMS = ("linux-x64", "linux-arm64", "darwin-arm64", "darwin-x64", "windows-x64")
RETAINED = 30


def gh(*args):
    return subprocess.check_output(["gh", *args], text=True)


def releases():
    pages = json.loads(gh("api", "--paginate", "--slurp", f"repos/{os.environ['GITHUB_REPOSITORY']}/releases"))
    return [release for page in pages for release in page]


def nightlies():
    """Published nightly releases, newest first."""
    published = [release for release in releases() if not release["draft"] and NIGHTLY.fullmatch(release["tag_name"])]
    return sorted(published, key=lambda release: release["published_at"], reverse=True)


def plan(sha, event):
    with open("Cargo.toml", "rb") as source:
        base = tomllib.load(source)["workspace"]["package"]["version"].split("-")[0]
    date = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%d")
    version = f"{base}-nightly.{date}.g{sha[:12]}"
    # Scheduled runs skip a commit the latest nightly already shipped; manual runs always build.
    latest = nightlies()[:1] if event == "schedule" else []
    build = not any(f"Source commit: {sha}" in (release["body"] or "") for release in latest)
    return version, build


def publish(directory, version, sha):
    tag = f"v{version}"
    assets = sorted(path for path in directory.iterdir() if path.is_file())
    archives = {f"chunk-{version}-{platform}.tar.gz" for platform in PLATFORMS}
    expected_assets = archives | {name + ".sha256" for name in archives}
    actual_assets = {path.name for path in assets}
    if actual_assets != expected_assets:
        raise ValueError(f"nightly assets differ: missing {sorted(expected_assets - actual_assets)}, "
                         f"unexpected {sorted(actual_assets - expected_assets)}")
    for path in assets:
        if path.suffix != ".sha256":
            expected = path.with_name(path.name + ".sha256").read_text().split()[0]
            if expected != hashlib.sha256(path.read_bytes()).hexdigest():
                raise ValueError(f"checksum mismatch: {path.name}")
    existing = [release for release in releases() if release["tag_name"] == tag]
    if existing and (existing[0]["target_commitish"] != sha or not existing[0]["draft"]):
        raise ValueError(f"{tag} already exists for another commit or is published")
    if not existing:
        gh("release", "create", tag, "--draft", "--prerelease", "--target", sha, "--title", f"Chunk {version}",
           "--notes", f"Nightly build of `main`.\n\nSource commit: {sha}\n")
    gh("release", "upload", tag, *map(str, assets), "--clobber")
    with tempfile.TemporaryDirectory() as temporary:
        gh("release", "download", tag, "--dir", temporary)
        downloaded = Path(temporary)
        if sorted(path.name for path in downloaded.iterdir()) != [path.name for path in assets]:
            raise ValueError("release assets differ from the verified archives")
        for path in assets:
            if path.read_bytes() != (downloaded / path.name).read_bytes():
                raise ValueError(f"uploaded asset differs: {path.name}")
    gh("release", "edit", tag, "--draft=false", "--prerelease", "--latest=false")
    for release in nightlies()[RETAINED:]:
        gh("release", "delete", release["tag_name"], "--yes", "--cleanup-tag")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["plan", "publish"])
    parser.add_argument("--directory", type=Path, default=Path("dist"))
    args = parser.parse_args()
    sha = os.environ["GITHUB_SHA"]
    if args.command == "publish":
        publish(args.directory, os.environ["CHUNK_RELEASE_VERSION"], sha)
        return
    version, build = plan(sha, os.environ["GITHUB_EVENT_NAME"])
    with open(os.environ["GITHUB_OUTPUT"], "a") as output:
        output.write(f"version={version}\nbuild={str(build).lower()}\n")


if __name__ == "__main__":
    main()
