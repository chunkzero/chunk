#!/usr/bin/env python3
"""Install an SDK archive and build real consumers without a Chunk source checkout."""

import argparse
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import runpy
import shutil
import subprocess
import tempfile
from threading import Thread


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    args = parser.parse_args()
    archive = args.archive.resolve(strict=True)
    repository = Path(__file__).resolve().parent.parent
    consumers = runpy.run_path(str(repository / "scripts/check-consumers.py"))
    with tempfile.TemporaryDirectory(prefix="chunk installed sdk ") as temporary:
        root = Path(temporary)
        version = archive.name.removeprefix("chunk-").removesuffix("-linux-x64.tar.gz")
        prefix = root / "installation"
        environment = dict(os.environ, CHUNK_INSTALL_DIR=str(prefix))
        environment.pop("CHUNK_TYPESCRIPT", None)
        subprocess.run(["sh", str(repository / "scripts/install.sh"), version, str(archive)],
                       env=environment, check=True)
        sdk = prefix / "share/chunk" / version
        metadata = json.loads((sdk / "sdk.json").read_text())
        executable = prefix / "bin/chunk"

        class Repository(SimpleHTTPRequestHandler):
            def __init__(self, *args, **kwargs):
                super().__init__(*args, directory=str(sdk / "sdk/maven"), **kwargs)

            def log_message(self, *_args):
                pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), Repository)
        thread = Thread(target=server.serve_forever)
        thread.start()
        try:
            url = f"http://127.0.0.1:{server.server_port}"
            for name, package, apps, kotlin in (
                ("java", "dev.chunkzero.generated", {"lobby"}, False),
                ("local", "dev.chunkzero.example.generated", {"lobby", "arena"}, True),
            ):
                project = root / name
                shutil.copytree(repository / "examples" / name, project,
                                ignore=shutil.ignore_patterns(*consumers["EXCLUDED"]))
                shutil.copytree(sdk / "sdk/wrapper", project, dirs_exist_ok=True)
                shutil.copy2(repository / "gradle/libs.versions.toml", project / "gradle/libs.versions.toml")
                kotlin_plugin = (f'id("org.jetbrains.kotlin.jvm") version "{metadata["kotlin_version"]}" apply false'
                                 if kotlin else "")
                settings = f'''import dev.chunkzero.gradle.ChunkSettingsExtension
pluginManagement {{
    repositories {{
        maven {{ url = uri("{url}"); isAllowInsecureProtocol = true }}
        gradlePluginPortal()
        mavenCentral()
    }}
}}
plugins {{
    {kotlin_plugin}
    id("dev.chunkzero.chunk.settings") version "{version}"
    id("org.gradle.toolchains.foojay-resolver-convention") version "{metadata["foojay_version"]}"
}}
extensions.configure<ChunkSettingsExtension> {{ javaPackage.set("{package}") }}
dependencyResolutionManagement {{
    repositories {{
        maven {{ url = uri("{url}"); isAllowInsecureProtocol = true }}
        mavenCentral()
    }}
}}
rootProject.name = "{name}"
'''
                if kotlin:
                    settings += 'include(":shared")\n'
                    # The fixture's shared module has source-checkout-only backend integration tests.
                    (project / "shared/build.gradle.kts").write_text('''plugins { id("dev.chunkzero.chunk.kotlin") }
java { toolchain.languageVersion = JavaLanguageVersion.of(25) }
''')
                (project / "settings.gradle.kts").write_text(settings)
                subprocess.run([str(executable), "codegen", str(project)], cwd=root, env=environment, check=True)
                subprocess.run([str(executable), "build", str(project)], cwd=root, env=environment, check=True)
                consumers["verify_release"](project, package, apps, kotlin)
        finally:
            server.shutdown()
            thread.join()
            server.server_close()
    print("Verified installed SDK: HTTP Maven resolution, Java and Kotlin consumers", flush=True)


if __name__ == "__main__":
    main()
