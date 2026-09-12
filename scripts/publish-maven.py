#!/usr/bin/env python3
"""Publish immutable Maven artifacts from an extracted SDK to Cloudflare R2."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def publish(sdk, account, bucket):
    metadata = json.loads((sdk / "sdk.json").read_text())
    version = metadata["version"]
    if metadata["schema"] != 1 or not re.fullmatch(r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?", version):
        raise ValueError("Invalid SDK metadata")
    if not re.fullmatch(r"[a-f0-9]{32}", account):
        raise ValueError("Invalid Cloudflare account ID")
    repository = sdk / "sdk/maven"
    paths = sorted(path for path in repository.rglob("*") if path.is_file())
    if not paths:
        raise ValueError("SDK contains no Maven artifacts")
    command = ["aws", "s3api", "--endpoint-url", f"https://{account}.r2.cloudflarestorage.com",
               "--region", "auto", "--no-cli-pager"]
    pending = []
    for path in paths:
        key = path.relative_to(repository).as_posix()
        if not key.startswith("dev/chunkzero/") or path.parent.name != version or path.is_symlink():
            raise ValueError(f"Expected an immutable versioned Chunk artifact: {key}")
        checksum = digest(path)
        result = subprocess.run(command + ["head-object", "--bucket", bucket, "--key", key],
                                capture_output=True, text=True, check=False)
        if result.returncode == 0:
            if json.loads(result.stdout).get("Metadata", {}).get("sha256") != checksum:
                raise ValueError(f"Refusing to replace published artifact: {key}")
        elif "(404)" in result.stderr or "(NotFound)" in result.stderr:
            pending.append((path, key, checksum))
        else:
            raise RuntimeError(result.stderr.strip())
    for path, key, checksum in pending:
        content_type = {".pom": "application/xml", ".module": "application/json",
                        ".jar": "application/java-archive"}.get(path.suffix, "text/plain")
        subprocess.run(command + [
            "put-object", "--bucket", bucket, "--key", key, "--body", str(path),
            "--metadata", f"sha256={checksum}", "--if-none-match", "*",
            "--content-type", content_type, "--cache-control", "public, max-age=31536000, immutable",
        ], check=True, stdout=subprocess.DEVNULL)
        print(f"Published {key}", flush=True)
    print(f"Maven SDK {version}: {len(pending)} uploaded, {len(paths) - len(pending)} already present", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("sdk", type=Path)
    args = parser.parse_args()
    publish(args.sdk.resolve(strict=True), os.environ["CLOUDFLARE_ACCOUNT_ID"], os.environ["R2_MAVEN_BUCKET"])


if __name__ == "__main__":
    main()
