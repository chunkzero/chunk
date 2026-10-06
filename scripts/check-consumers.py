#!/usr/bin/env python3
"""Build real Java and Kotlin consumers using a prepared Chunk CLI, without starting gameplay."""

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
from zipfile import ZipFile


EXCLUDED = (".git", "build", ".chunk", ".gradle", ".kotlin", "target", "dist", "node_modules")
INPUTS = (
    "gradlew", "gradlew.bat", "gradle", "gradle.properties", "settings.gradle.kts", "build.gradle.kts",
    "buildSrc", "jvm", "proto", "examples/arena", "examples/local",
)
PROVIDER = "META-INF/services/com.chunkzero.chunk.multistom.SessionProvider"


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def copy_sources(repository, destination):
    for name in INPUTS:
        source = repository / name
        target = destination / name
        target.parent.mkdir(parents=True, exist_ok=True)
        if source.is_dir():
            shutil.copytree(source, target, ignore=shutil.ignore_patterns(*EXCLUDED), symlinks=True)
        else:
            shutil.copy2(source, target)


def read_json(path):
    return json.loads(path.read_text())


def verify_release(project, package, app_ids, kotlin):
    descriptor = read_json(project / ".chunk/build/jvm/artifacts.json")
    require(descriptor["version"] == 4, "Unsupported JVM descriptor")
    require({app["id"] for app in descriptor["apps"]} == app_ids, "Unexpected descriptor apps")
    compiled_apps = {app["id"]: Path(app["jar"]) for app in descriptor["apps"]}
    require(all(path.is_file() for path in compiled_apps.values()), "Missing compiled app JAR")
    archives = list((project / "dist").glob("*.tar.gz"))
    require(len(archives) == 1, "Expected one release from a clean consumer build")
    archive = archives[0]
    release = archive.with_name(archive.name.removesuffix(".tar.gz"))
    manifest = read_json(release / "release.json")
    require(manifest["version"] == 4 and manifest["id"] == release.name, "Invalid release identity")
    require(read_json(release / "backend.json")["id"] == release.name, "Backend identity differs from release")
    require(manifest["java_version"] == descriptor["java"]["version"], "Java requirement differs from descriptor")
    require({app["id"] for app in manifest["apps"]} == app_ids, "Unexpected release apps")
    require(str(project) not in json.dumps(manifest), "Release contains a local project path")
    payloads = {path.relative_to(release).as_posix() for path in release.rglob("*") if path.is_file()}
    with tarfile.open(archive, "r:gz") as tar:
        members = tar.getmembers()
        require(len(members) == len(payloads), "Archive contains duplicate or extra entries")
        require({member.name for member in members} == payloads, "Archive payloads differ from release directory")
        for member in members:
            require(member.isfile(), f"Unexpected archive entry: {member.name}")
            with tar.extractfile(member) as source:
                expected = hashlib.sha256((release / member.name).read_bytes()).digest()
                require(hashlib.sha256(source.read()).digest() == expected, f"Archive differs: {member.name}")

    for app in manifest["apps"]:
        path = release / app["jar"]
        require(hashlib.sha256(path.read_bytes()).hexdigest() == app["sha256"], "App JAR hash differs")
        require(hashlib.sha256(compiled_apps[app["id"]].read_bytes()).hexdigest() == app["sha256"],
                "Release app differs from the compiled descriptor input")
        with ZipFile(path) as jar:
            names = jar.namelist()
            require(len(names) == len(set(names)), "Executable contains duplicate ZIP entries")
            require("META-INF/chunk/app.json" not in names and names.count(PROVIDER) == 1,
                    "Executable must contain a local service registry without a deployment manifest")
            compiled = next(item for item in descriptor["apps"] if item["id"] == app["id"])
            require(set(app["sessions"]) == set(compiled["sessions"]), "Session types differ from the descriptor")
            require("manifest" not in app and "manifest_digest" not in app, "Embedded app metadata remains")
            require(all(set(session) == {"machine_profile", "capacity"} for session in app["sessions"].values()),
                    "Release session declarations contain JVM implementation details")
            attributes = jar.read("META-INF/MANIFEST.MF").decode().replace("\r\n ", "")
            mains = [line.split(":", 1)[1].strip() for line in attributes.splitlines()
                     if line.lower().startswith("main-class:")]
            require(len(mains) == 1 and mains[0].replace(".", "/") + ".class" in names, "Main class missing")
            providers = [line.strip() for line in jar.read(PROVIDER).decode().splitlines() if line.strip()]
            require(len(providers) == len(app["sessions"]), "Session registry differs from deployment capabilities")
            for provider in providers:
                require(provider.replace(".", "/") + ".class" in names, "Session factory missing")
            for entry in ("com/chunkzero/chunk/runtime/ChunkProcess.class", "com/chunkzero/chunk/multistom/ChunkMinestom.class",
                          "net/minestom/server/ServerProcess.class", package.replace(".", "/") + "/BackendTypes.class",
                          package.replace(".", "/") + "/BackendClient.class"):
                require(entry in names, f"App executable missing {entry}")
            require("com/chunkzero/chunk/runtime/BridgeMain.class" not in names, "Legacy runtime launcher remains")
            facade = package.replace(".", "/") + "/CoroutineBackendClient.class"
            require((facade in names) == kotlin, "Unexpected Kotlin facade")
            if not kotlin:
                require(not any(name.startswith(("kotlin/", "kotlinx/")) for name in names), "Java app contains Kotlin production classes")
    require("classpath" not in manifest, "Apps must carry their own dependencies")
    print(f"Verified {project.name}: {len(app_ids)} independent executable app(s) with generated session catalogs", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("chunk", type=Path, help="Prepared CLI executable with its native TypeScript toolchain")
    arguments = parser.parse_args()
    executable = arguments.chunk.resolve(strict=True)
    repository = Path(__file__).resolve().parent.parent
    require((repository / "examples/local/shared/build.gradle.kts").is_file(), "The Kotlin example requires its :shared module")
    with tempfile.TemporaryDirectory(prefix="chunk consumers ") as temporary:
        checkout = Path(temporary) / "source checkout"
        copy_sources(repository, checkout)
        subprocess.run([str(checkout / "gradlew"), "assemble", "--no-daemon", "--max-workers=2", "--console=plain"],
                       cwd=checkout, check=True)
        for language in ("java", "kotlin"):
            project = Path(temporary) / f"new {language} server"
            subprocess.run([str(executable), "create", str(project), "--language", language,
                            "--chunk-source", str(checkout)], check=True)
            require(not (project / ".chunk").exists(), "Scaffolding should not depend on generated output")
            subprocess.run([str(executable), "codegen", str(project)], check=True)
            subprocess.run([str(executable), "build", str(project)], check=True)
            verify_release(project, "com.chunkzero.chunk.generated", {"lobby"}, language == "kotlin")
        for name, package, apps, kotlin in (
            ("arena", "com.chunkzero.chunk.generated", {"arena", "lobby"}, False),
            ("local", "com.chunkzero.chunk.example.generated", {"arena", "lobby"}, True),
        ):
            project = checkout / "examples" / name
            require(not (project / ".chunk").exists() and not (project / "dist").exists(), "Consumer outputs were copied")
            subprocess.run([str(executable), "build", "--frozen", str(project)], cwd=checkout, check=True)
            verify_release(project, package, apps, kotlin)


if __name__ == "__main__":
    main()
