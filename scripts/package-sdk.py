#!/usr/bin/env python3
"""Assemble a versioned Linux x64 SDK from a prepared CLI and the JVM publications."""

import argparse
import gzip
import hashlib
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tarfile
import tempfile
import tomllib


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--chunk", type=Path, default=Path("target/release/chunk"))
    parser.add_argument("--output", type=Path, default=Path("target/dist"))
    args = parser.parse_args()
    repository = Path(__file__).resolve().parent.parent
    executable = args.chunk.resolve(strict=True)
    output = args.output.resolve()
    cargo = tomllib.loads((repository / "Cargo.toml").read_text())
    versions = tomllib.loads((repository / "gradle/libs.versions.toml").read_text())["versions"]
    version = cargo["workspace"]["package"]["version"]
    if not re.fullmatch(r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?", version):
        raise ValueError("SDK releases require a semantic version")
    if versions["chunk"] != version:
        raise ValueError("Cargo and JVM SDK versions differ")
    actual = subprocess.check_output([str(executable), "--version"], text=True).strip()
    if actual != f"chunk {version}":
        raise ValueError(f"Expected chunk {version}, got {actual}")
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        raise ValueError("The initial SDK supports Linux x64")
    name = f"chunk-{version}-linux-x64"
    output.mkdir(parents=True, exist_ok=True)
    archive = output / f"{name}.tar.gz"
    maven = output / "maven"
    if archive.exists() or maven.exists():
        raise FileExistsError(f"Refusing to replace SDK outputs in {output}; use a fresh output directory")
    with tempfile.TemporaryDirectory(prefix=".sdk-", dir=output) as temporary:
        root = Path(temporary) / name
        staged_maven = Path(temporary) / "maven"
        root.mkdir()
        shutil.copy2(executable, root / "chunk")
        shutil.copy2(repository / "LICENSE.md", root / "LICENSE.md")
        subprocess.run(["node", "scripts/install-typescript.mjs", str(root)], cwd=repository, check=True)
        subprocess.run([
            str(repository / "gradlew"), "publishSdk", f"-Pchunk.sdkRepository={staged_maven}",
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
                    info = tar.gettarinfo(path, arcname=path.relative_to(root.parent))
                    info.uid = info.gid = info.mtime = 0
                    info.uname = info.gname = ""
                    with path.open("rb") as source:
                        tar.addfile(info, source)
        staged_maven.rename(maven)
        staged_archive.rename(archive)
    with archive.open("rb") as source:
        checksum = hashlib.file_digest(source, "sha256").hexdigest()
    archive.with_suffix(archive.suffix + ".sha256").write_text(f"{checksum}  {archive.name}\n")
    print(archive, flush=True)


if __name__ == "__main__":
    main()
