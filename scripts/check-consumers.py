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
    "buildSrc", "jvm", "proto", "examples/java", "examples/local",
)
APP_MANIFEST = "META-INF/chunk/app.json"
PROVIDER = "META-INF/services/dev.chunkzero.runtime.SessionProvider"


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
    require(descriptor["version"] == 1, "Unsupported JVM descriptor")
    require({app["id"] for app in descriptor["apps"]} == app_ids, "Unexpected descriptor apps")
    compiled_apps = {app["id"]: Path(app["jar"]) for app in descriptor["apps"]}
    require(all(path.is_file() for path in compiled_apps.values()), "Missing compiled app JAR")
    archives = list((project / "dist").glob("*.tar.gz"))
    require(len(archives) == 1, "Expected one release from a clean consumer build")
    archive = archives[0]
    release = archive.with_name(archive.name.removesuffix(".tar.gz"))
    manifest = read_json(release / "release.json")
    require(manifest["version"] == 1 and manifest["id"] == release.name, "Invalid release identity")
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

    jars = {}
    for app in manifest["apps"]:
        path = release / app["jar"]
        require(hashlib.sha256(path.read_bytes()).hexdigest() == app["sha256"], "App JAR hash differs")
        require(hashlib.sha256(compiled_apps[app["id"]].read_bytes()).hexdigest() == app["sha256"],
                "Release app differs from the compiled descriptor input")
        with ZipFile(path) as jar:
            names = jar.namelist()
            require(names.count(APP_MANIFEST) == 1 and names.count(PROVIDER) == 1, "App registration missing or repeated")
            require(json.loads(jar.read(APP_MANIFEST)) == {"version": 1, "id": app["id"]}, "Wrong app JAR identity")
            providers = [line.split("#", 1)[0].strip() for line in jar.read(PROVIDER).decode().splitlines()]
            providers = [name for name in providers if name]
            require(len(providers) == 1, "Expected one provider per app")
            require(providers[0].replace(".", "/") + ".class" in names, "Provider belongs to another JAR")
            jars[app["jar"]] = set(names)
    for dependency in manifest["classpath"]:
        path = release / dependency["file"]
        require(hashlib.sha256(path.read_bytes()).hexdigest() == dependency["sha256"], "Dependency JAR hash differs")
        with ZipFile(path) as jar:
            names = set(jar.namelist())
            require(APP_MANIFEST not in names and PROVIDER not in names, "Shared JAR registers an app")
            jars[dependency["file"]] = names

    require(sum("dev/chunkzero/runtime/BridgeMain.class" in entries for entries in jars.values()) == 1,
            "Release must contain the gameplay runtime")
    types = package.replace(".", "/") + "/BackendTypes"
    bindings = [name for name, entries in jars.items() if types + ".class" in entries]
    require(len(bindings) == 1, "Expected one Java model JAR")
    shared = [entry for entry in manifest["classpath"] if entry["artifact"] == "chunk-backend.jar"]
    require(len(shared) == 1 and shared[0]["file"] == bindings[0], "Models must belong to the shared bindings JAR")
    require(package.replace(".", "/") + "/BackendClient.class" in jars[bindings[0]], "Java client missing from bindings JAR")
    require(all(not any(name.startswith(types) for name in entries) for jar, entries in jars.items() if jar != bindings[0]),
            "Java models were duplicated outside the shared bindings JAR")
    facade = package.replace(".", "/") + "/CoroutineBackendClient.class"
    facades = [name for name, entries in jars.items() if facade in entries]
    require(len(facades) == int(kotlin), "Unexpected Kotlin facade JAR count")
    if kotlin:
        require(facades[0] != bindings[0], "Kotlin facade must be separate from Java models")
        require(any(entry["component"].get("path") == ":shared" for entry in manifest["classpath"]),
                "Kotlin apps must use the standalone shared example module")
    else:
        require(not any(name.startswith(("kotlin/", "kotlinx/")) for entries in jars.values() for name in entries),
                "Java consumer has Kotlin production classes")
        require(not any("kotlin" in entry["artifact"].lower() for entry in manifest["classpath"]),
                "Java consumer has Kotlin production dependencies")
    print(f"Verified {project.name}: {len(app_ids)} app(s), one Java bindings JAR, {len(facades)} Kotlin facade(s)", flush=True)


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
        for name, package, apps, kotlin in (
            ("java", "dev.chunkzero.generated", {"lobby"}, False),
            ("local", "dev.chunkzero.example.generated", {"arena", "lobby"}, True),
        ):
            project = checkout / "examples" / name
            require(not (project / ".chunk").exists() and not (project / "dist").exists(), "Consumer outputs were copied")
            subprocess.run([str(executable), "build", str(project)], cwd=checkout, check=True)
            verify_release(project, package, apps, kotlin)


if __name__ == "__main__":
    main()
