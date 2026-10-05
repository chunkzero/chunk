#!/usr/bin/env python3
"""Assemble a versioned SDK for one platform from a prepared CLI and, optionally, the JVM publications."""

import argparse
import gzip
import hashlib
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib

EXECUTABLES = {"chunk", "chunk.exe", "tsc", "tsc.exe"}


def host_platform():
    system = {"Linux": "linux", "Darwin": "darwin", "Windows": "windows"}.get(platform.system())
    arch = {"x86_64": "x64", "AMD64": "x64", "aarch64": "arm64", "arm64": "arm64"}.get(platform.machine())
    if system is None or arch is None:
        raise ValueError(f"Unsupported SDK platform: {platform.system()} {platform.machine()}")
    return f"{system}-{arch}"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--chunk", type=Path, help="CLI to package; defaults to Cargo's release output")
    parser.add_argument("--platform", default=host_platform(), help="platform of the CLI; defaults to the host")
    parser.add_argument("--output", type=Path, default=Path("target/dist"))
    parser.add_argument("--no-maven", action="store_true", help="package only the CLI archive")
    args = parser.parse_args()
    target = args.platform
    suffix = ".exe" if target.startswith("windows-") else ""
    # Cargo writes cross-compiled builds to a target triple directory.
    rust_target = os.environ.get("CARGO_BUILD_TARGET", "")
    chunk = args.chunk or Path(os.environ.get("CARGO_TARGET_DIR", "target"), rust_target, "release", f"chunk{suffix}")
    repository = Path(__file__).resolve().parent.parent
    executable = chunk.resolve(strict=True)
    output = args.output.resolve()
    cargo = tomllib.loads((repository / "Cargo.toml").read_text())
    versions = tomllib.loads((repository / "gradle/libs.versions.toml").read_text())["versions"]
    base = cargo["workspace"]["package"]["version"]
    # Releases use the workspace version; nightlies extend its X.Y.Z with a `-nightly.<time>.g<commit>` suffix.
    version = os.environ.get("CHUNK_RELEASE_VERSION") or base
    nightly = re.fullmatch(r"(\d+\.\d+\.\d+)-nightly\.\d{14}\.g[0-9a-f]{12}", version)
    if version != base and not (nightly and nightly[1] == base.split("-")[0]):
        raise ValueError(f"SDK releases require a semantic version of {base}, got {version}")
    if versions["chunk"] != base:
        raise ValueError("Cargo and JVM SDK versions differ")
    actual = subprocess.check_output([str(executable), "--version"], text=True).strip()
    if actual != f"chunk {version}":
        raise ValueError(f"Expected chunk {version}, got {actual}")
    name = f"chunk-{version}-{target}"
    output.mkdir(parents=True, exist_ok=True)
    archive = output / f"{name}.tar.gz"
    maven = output / "maven"
    if archive.exists() or (not args.no_maven and maven.exists()):
        raise FileExistsError(f"Refusing to replace SDK outputs in {output}; use a fresh output directory")
    with tempfile.TemporaryDirectory(prefix=".sdk-", dir=output) as temporary:
        root = Path(temporary) / name
        staged_maven = Path(temporary) / "maven"
        root.mkdir()
        shutil.copy2(executable, root / f"chunk{suffix}")
        shutil.copy2(repository / "LICENSE.md", root / "LICENSE.md")
        notices = subprocess.run([sys.executable, "-X", "utf8", "scripts/rust-notices.py",
                                  *(["--target", rust_target] if rust_target else [])], cwd=repository, check=True,
                                 stdout=subprocess.PIPE, text=True, encoding="utf-8").stdout
        (root / "THIRD_PARTY_LICENSES").write_text(notices, encoding="utf-8", newline="\n")
        subprocess.run(["node", "scripts/install-typescript.mjs", str(root), target], cwd=repository, check=True)
        if not args.no_maven:
            subprocess.run([
                str(repository / "gradlew"), "publishSdk", f"-Pchunk.sdkRepository={staged_maven}",
                f"-Pchunk.version={version}",
                "--max-workers=2", "--console=plain", "--no-daemon",
            ], cwd=repository, check=True)
            # Exact versions need no mutable repository-level Maven version indexes.
            for path in staged_maven.rglob("maven-metadata.xml*"):
                path.unlink()
        staged_archive = Path(temporary) / archive.name
        with staged_archive.open("xb") as raw, gzip.GzipFile(filename="", fileobj=raw, mode="wb", mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w") as tar:
                for path in sorted(root.rglob("*")):
                    if not path.is_file():
                        continue
                    info = tar.gettarinfo(path, arcname=path.relative_to(root.parent).as_posix())
                    info.uid = info.gid = info.mtime = 0
                    info.uname = info.gname = ""
                    # Windows has no executable bit to carry over.
                    info.mode = 0o755 if path.name in EXECUTABLES else 0o644
                    with path.open("rb") as source:
                        tar.addfile(info, source)
        if not args.no_maven:
            staged_maven.rename(maven)
        staged_archive.rename(archive)
    with archive.open("rb") as source:
        checksum = hashlib.file_digest(source, "sha256").hexdigest()
    archive.with_suffix(archive.suffix + ".sha256").write_text(f"{checksum}  {archive.name}\n", newline="\n")
    print(archive, flush=True)


if __name__ == "__main__":
    main()
