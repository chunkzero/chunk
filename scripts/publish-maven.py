#!/usr/bin/env python3
"""Stage verified Maven artifacts through the Maven R2 CLI's local publication proxy."""

import argparse
import base64
from contextlib import closing
from http.client import HTTPConnection
import os
from pathlib import Path
import re
from urllib.parse import urlsplit


def publish(repository, version, url, username, password):
    if not re.fullmatch(r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?", version):
        raise ValueError("Invalid SDK version")
    proxy = urlsplit(url)
    if (proxy.scheme != "http" or proxy.hostname != "127.0.0.1" or not proxy.port
            or proxy.username or proxy.password or proxy.path or proxy.query or proxy.fragment):
        raise ValueError("Expected MAVEN_R2_URL from the local Maven R2 publication proxy")
    if not username or not password:
        raise ValueError("Missing Maven R2 proxy credentials")
    paths = []
    for path in sorted(repository.rglob("*")):
        key = path.relative_to(repository).as_posix()
        if path.is_symlink():
            raise ValueError(f"Symlink in Maven repository: {key}")
        if path.is_dir():
            continue
        if (not path.is_file() or not key.startswith("dev/chunkzero/") or path.parent.name != version
                or not all(re.fullmatch(r"[A-Za-z0-9_+.-]+", part) for part in path.relative_to(repository).parts)):
            raise ValueError(f"Expected an immutable versioned Chunk artifact: {key}")
        paths.append((path, key))
    if not paths:
        raise ValueError("Repository contains no Maven artifacts")
    authorization = base64.b64encode(f"{username}:{password}".encode()).decode()
    for path, key in paths:
        content_type = {".pom": "application/xml", ".module": "application/json",
                        ".jar": "application/java-archive"}.get(path.suffix, "text/plain")
        with closing(HTTPConnection(proxy.hostname, proxy.port, timeout=300)) as connection:
            with path.open("rb") as source:
                connection.request("PUT", f"/{key}", body=source, headers={
                    "Authorization": f"Basic {authorization}",
                    "Content-Length": str(path.stat().st_size),
                    "Content-Type": content_type,
                })
                response = connection.getresponse()
                if not 200 <= response.status < 300:
                    raise RuntimeError(f"Maven R2 rejected {key}: HTTP {response.status}")
        print(f"Staged {key}", flush=True)
    print(f"Maven SDK {version}: {len(paths)} files staged; the Maven R2 CLI commits the session", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repository", type=Path)
    parser.add_argument("version")
    args = parser.parse_args()
    publish(args.repository.resolve(strict=True), args.version,
            os.environ["MAVEN_R2_URL"], os.environ["MAVEN_R2_USERNAME"], os.environ["MAVEN_R2_PASSWORD"])


if __name__ == "__main__":
    main()
