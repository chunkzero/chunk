#!/usr/bin/env python3
"""Install an SDK archive and build real consumers without a Chunk source checkout."""

import argparse
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import runpy
import subprocess
import tempfile
from threading import Thread
from zipfile import ZipFile


def verify_documentation(repository):
    for module_file in repository.rglob("*.module"):
        module = json.loads(module_file.read_text())
        component = module["component"]
        for classifier in ("sources", "javadoc"):
            name = f'{component["module"]}-{component["version"]}-{classifier}.jar'
            variants = [variant for variant in module["variants"]
                        if variant["attributes"].get("org.gradle.docstype") == classifier]
            assert any(file["name"] == name for variant in variants for file in variant["files"]), name
            with ZipFile(module_file.with_name(name)) as jar:
                entries = jar.namelist()
                if classifier == "sources":
                    assert any(entry.endswith((".java", ".kt")) for entry in entries), name
                else:
                    assert "index.html" in entries, name


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    parser.add_argument("maven_repository", type=Path, help="Unpublished JVM artifacts to serve over HTTP")
    args = parser.parse_args()
    archive = args.archive.resolve(strict=True)
    maven = args.maven_repository.resolve(strict=True)
    verify_documentation(maven)
    repository = Path(__file__).resolve().parent.parent
    consumers = runpy.run_path(str(repository / "scripts/check-consumers.py"))
    with tempfile.TemporaryDirectory(prefix="chunk installed sdk ") as temporary:
        root = Path(temporary)
        version = archive.name.removeprefix("chunk-").removesuffix("-linux-x64.tar.gz")
        prefix = root / "installation"
        environment = dict(os.environ, CHUNK_INSTALL_DIR=str(prefix))
        environment.pop("CHUNK_TYPESCRIPT", None)
        environment.pop("CHUNK_SOURCE", None)
        subprocess.run(["sh", str(repository / "scripts/install.sh"), version, str(archive)],
                       env=environment, check=True)
        sdk = prefix / "share/chunk" / version
        executable = prefix / "bin/chunk"
        assert {path.name for path in sdk.iterdir()} == {"chunk", "LICENSE.md", "THIRD_PARTY_LICENSES", "toolchain"}, \
            "Only the CLI, native TypeScript toolchain, license and third-party notices should be installed"
        notices = (sdk / "THIRD_PARTY_LICENSES").read_text()
        for attribution in ("https://crates.io/crates/deno_core/", "Copyright (c) 2016 Dropbox, Inc.",
                            "Copyright (c) 2023 Boshen", "libdeflate/COPYING", "The Apache Software Foundation",
                            "src/unicode_tables/LICENSE-UNICODE", "rust-lang/libm as a whole", "`v8` crate",
                            "Copyright 2014, the V8 project authors", "UNICODE LICENSE V3",
                            # Notices carried only in source file headers.
                            "Dmitry Vyukov", "Gotham Project Developers", "Joyent, Inc. and other Node contributors",
                            "Domenic Denicola", "David Judd", "Daniel McCarney", "Radford M. Neal",
                            "the Dart project authors",
                            # Notices inside embedded code, split across comments, or with wrapped license terms.
                            "regenerator-runtime -- Copyright (c) 2014-present, Facebook, Inc.",
                            "Copyright (c) 2021-2022 Alexei Sibidanov.", "Copyright 2017, Twitter Inc.",
                            # Third-party code crates embed, credited only in their documentation.
                            "Copyright (c) 2014-present Sebastian McKenzie and other contributors",
                            "Copyright Node.js contributors.", "by the Brotli Authors", "Alexis Deveria",
                            # License texts that source notices refer to.
                            "Copyright (c) 2014-present, Facebook, Inc.\n\nPermission",
                            "// Copyright 2015 The Chromium Authors",
                            "Copyright 2009 The Go Authors.\n\nRedistribution",
                            "Copyright 2012, the Dart project authors.", "The BSD 2-Clause License"):
            assert attribution in notices, f"Third-party notices are missing {attribution!r}"
        assert not list(sdk.rglob("*.jar")), "JVM libraries must be resolved from Maven"

        class Repository(SimpleHTTPRequestHandler):
            def __init__(self, *args, **kwargs):
                super().__init__(*args, directory=str(maven), **kwargs)

            def log_message(self, *_args):
                pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), Repository)
        thread = Thread(target=server.serve_forever)
        thread.start()
        try:
            url = f"http://127.0.0.1:{server.server_port}"
            for language in ("java", "kotlin"):
                project = root / f"new {language} server"
                subprocess.run([str(executable), "create", str(project), "--language", language],
                               cwd=root, env=environment, check=True)
                with (project / "gradle.properties").open("a") as properties:
                    properties.write(f"\nchunk.mavenRepository={url}\n")
                subprocess.run([str(executable), "codegen", str(project)], cwd=root, env=environment, check=True)
                subprocess.run([str(executable), "build", str(project)], cwd=root, env=environment, check=True)
                consumers["verify_release"](project, "dev.chunkzero.generated", {"lobby"}, language == "kotlin")
        finally:
            server.shutdown()
            thread.join()
            server.server_close()
    print("Verified installed SDK: HTTP Maven artifacts and Java/Kotlin create → codegen → build", flush=True)


if __name__ == "__main__":
    main()
